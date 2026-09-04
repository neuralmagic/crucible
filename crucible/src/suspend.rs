//! Leaving and re-entering a run at an approval gate.
//!
//! A suspended run writes everything a later process needs beside the session log: the
//! `run-workspace` artifact (the state dir minus the session log, a bundle of the workspace
//! repo, and `resume.json`), and posts it with the session log to the controller's drop-box
//! when the pod carries the ingest env. A resumed pod restores the two artifacts into place
//! before the engine starts (`crucible fetch-resume`), so `plan run --resume` only ever sees
//! files.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use crucible_contract::ArtifactKind;
use serde::{Deserialize, Serialize};

use crate::ingest_client::{IngestConfig, fetch_artifact, post_artifact, resume_of_from_env};

/// The bundle of the workspace repository inside the `run-workspace` tar.
pub const WORKSPACE_BUNDLE: &str = "workspace.bundle";
/// The resume record inside `state/` and the `run-workspace` tar.
pub const RESUME_FILE: &str = "resume.json";

/// What a suspended run leaves for its successor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeRecord {
    pub v: u8,
    pub run_id: String,
    /// The gate the run suspended on.
    pub gate: String,
    /// `HEAD` of the workspace at suspend time; the bundle carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Unix seconds.
    pub suspended_at: f64,
}

impl ResumeRecord {
    pub const VERSION: u8 = 1;
}

#[derive(Debug, thiserror::Error)]
pub enum SuspendError {
    #[error("the run-workspace snapshot is {bytes} bytes, over the {cap} byte cap")]
    Oversize { bytes: u64, cap: u64 },
    #[error("posting {kind} to the drop-box failed")]
    Undelivered { kind: ArtifactKind },
    #[error(
        "fetch-resume needs the pod ingest env ({})",
        crucible_contract::ENV_INGEST_URL
    )]
    NoIngestEnv,
    #[error(
        "fetch-resume needs {} to name the pod to resume",
        crucible_contract::ENV_RESUME_OF
    )]
    NoResumeOf,
    #[error("fetching {kind} of pod {of}: {message}")]
    Fetch {
        kind: ArtifactKind,
        of: String,
        message: String,
    },
    #[error("pod {of} left no {kind} artifact")]
    MissingArtifact { kind: ArtifactKind, of: String },
}

/// Attempts a drop-box fetch gets before the resume gives up; the controller may still be
/// storing the artifact when a fast scheduler starts the successor pod.
const FETCH_ATTEMPTS: u32 = 3;
const FETCH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Build the `run-workspace` artifact (gzipped tar) for `state` and `workspace`, and write the
/// resume record into `state/` so a local resume finds it too.
pub fn snapshot(state: &Path, workspace: &Path, record: &ResumeRecord) -> Result<Vec<u8>> {
    let record_json = serde_json::to_vec_pretty(record).context("encoding resume.json")?;
    std::fs::write(state.join(RESUME_FILE), &record_json)
        .with_context(|| format!("writing {}", state.join(RESUME_FILE).display()))?;
    let bundle = bundle_workspace(workspace)?;

    let mut builder = tar::Builder::new(Vec::new());
    append_dir(&mut builder, state, Path::new("state"), &|name| {
        name != "session.jsonl" && name != "files.tmp"
    })?;
    if let Some(bundle) = bundle {
        let mut header = tar::Header::new_gnu();
        header.set_size(bundle.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, WORKSPACE_BUNDLE, bundle.as_slice())
            .context("adding the workspace bundle")?;
    }
    let tar = builder.into_inner().context("finishing the snapshot tar")?;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar).context("gzipping the snapshot")?;
    let gz = enc.finish().context("gzipping the snapshot")?;
    let cap = ArtifactKind::RunWorkspace.max_bytes();
    if gz.len() as u64 > cap {
        return Err(SuspendError::Oversize {
            bytes: gz.len() as u64,
            cap,
        }
        .into());
    }
    Ok(gz)
}

/// Post the session log and the snapshot to the drop-box. `Ok(false)` when the pod carries no
/// ingest env (a local run keeps its state dir), `Err` when a post did not land.
pub fn deliver(session_log: &Path, snapshot: &[u8]) -> Result<bool> {
    let Some(cfg) = IngestConfig::from_env() else {
        return Ok(false);
    };
    let session =
        std::fs::read(session_log).with_context(|| format!("reading {}", session_log.display()))?;
    let session_gz = gzip(&session)?;
    for (kind, bytes) in [
        (ArtifactKind::RunSession, session_gz.as_slice()),
        (ArtifactKind::RunWorkspace, snapshot),
    ] {
        if !post_artifact(&cfg, kind, bytes).delivered {
            return Err(SuspendError::Undelivered { kind }.into());
        }
    }
    Ok(true)
}

/// Restore a `run-workspace` artifact into `state` and `workspace`: the state files go back
/// where they were, the bundle is fetched into the workspace repo, and its `HEAD` checked out.
pub fn restore(snapshot_gz: &[u8], state: &Path, workspace: &Path) -> Result<Option<ResumeRecord>> {
    let tar = {
        let mut dec = flate2::read::GzDecoder::new(snapshot_gz);
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut out).context("gunzipping the snapshot")?;
        out
    };
    let mut archive = tar::Archive::new(tar.as_slice());
    let mut bundle: Option<Vec<u8>> = None;
    std::fs::create_dir_all(state).with_context(|| format!("creating {}", state.display()))?;
    for entry in archive.entries().context("reading the snapshot tar")? {
        let mut entry = entry.context("reading a snapshot entry")?;
        let path = entry.path().context("snapshot entry path")?.into_owned();
        if path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            anyhow::bail!("snapshot entry {} escapes the state dir", path.display());
        }
        if path == Path::new(WORKSPACE_BUNDLE) {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).context("reading the bundle")?;
            bundle = Some(bytes);
            continue;
        }
        let Ok(rel) = path.strip_prefix("state") else {
            continue;
        };
        let to = state.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        entry
            .unpack(&to)
            .with_context(|| format!("restoring {}", to.display()))?;
    }
    let record = std::fs::read(state.join(RESUME_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ResumeRecord>(&bytes).ok());
    if let Some(bundle) = bundle {
        restore_bundle(
            workspace,
            &bundle,
            record.as_ref().and_then(|r| r.head.as_deref()),
        )?;
    }
    Ok(record)
}

/// Restore a suspended run's artifacts from the drop-box into `into`, the receiving half of
/// [`deliver`]: `run-session` becomes `<into>/state/session.jsonl` and `run-workspace` is
/// unpacked over `<into>/state` and `<into>/<workspace>`. Reads the pod ingest env and
/// `CRUCIBLE_RESUME_OF`, so it runs as an init container beside the engine it feeds.
pub fn fetch_resume(into: &Path, workspace: &str) -> Result<Option<ResumeRecord>> {
    let cfg = IngestConfig::from_env().ok_or(SuspendError::NoIngestEnv)?;
    let of = resume_of_from_env().ok_or(SuspendError::NoResumeOf)?;
    let state = into.join("state");
    std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;

    let session = fetch_required(&cfg, &of, ArtifactKind::RunSession)?;
    let log = state.join("session.jsonl");
    std::fs::write(&log, gunzip(&session)?)
        .with_context(|| format!("writing {}", log.display()))?;

    let snapshot = fetch_required(&cfg, &of, ArtifactKind::RunWorkspace)?;
    restore(&snapshot, &state, &into.join(workspace))
}

/// One artifact, retried: a 404 means the controller has not stored it yet, an error means the
/// call itself failed. Both are worth another attempt before refusing to resume.
fn fetch_required(cfg: &IngestConfig, of: &str, kind: ArtifactKind) -> Result<Vec<u8>> {
    let mut last: Option<String> = None;
    for attempt in 1..=FETCH_ATTEMPTS {
        match fetch_artifact(cfg, of, kind) {
            Ok(Some(bytes)) => return Ok(bytes),
            Ok(None) => last = None,
            Err(e) => last = Some(e),
        }
        if attempt < FETCH_ATTEMPTS {
            std::thread::sleep(FETCH_BACKOFF);
        }
    }
    match last {
        Some(message) => Err(SuspendError::Fetch {
            kind,
            of: of.to_string(),
            message,
        }
        .into()),
        None => Err(SuspendError::MissingArtifact {
            kind,
            of: of.to_string(),
        }
        .into()),
    }
}

fn bundle_workspace(workspace: &Path) -> Result<Option<Vec<u8>>> {
    if !workspace.join(".git").exists() {
        return Ok(None);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["bundle", "create", "-", "--all"])
        .output()
        .context("running git bundle")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A repository with no commits cannot be bundled; the resumed run starts from the
        // pack's own setup, which is what it had.
        if stderr.contains("Refusing to create empty bundle") {
            return Ok(None);
        }
        anyhow::bail!("git bundle failed: {}", stderr.trim());
    }
    Ok(Some(out.stdout))
}

fn restore_bundle(workspace: &Path, bundle: &[u8], head: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(workspace)
        .with_context(|| format!("creating {}", workspace.display()))?;
    let path = workspace.join(".crucible-resume.bundle");
    std::fs::write(&path, bundle).with_context(|| format!("writing {}", path.display()))?;
    if !workspace.join(".git").exists() {
        run_git(workspace, &["init", "-q"])?;
    }
    run_git(
        workspace,
        &[
            "fetch",
            "-q",
            path.to_string_lossy().as_ref(),
            "+refs/heads/*:refs/resume/*",
            "+refs/tags/*:refs/tags/*",
        ],
    )?;
    if let Some(head) = head {
        run_git(workspace, &["checkout", "-q", "--force", head])?;
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn run_git(workspace: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The workspace's `HEAD`, when it is a repository with a commit.
pub fn head_of(workspace: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn append_dir(
    builder: &mut tar::Builder<Vec<u8>>,
    dir: &Path,
    prefix: &Path,
    keep: &dyn Fn(&str) -> bool,
) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !keep(name) {
            continue;
        }
        let rel = prefix.join(name);
        if path.is_dir() {
            append_dir(builder, &path, &rel, keep)?;
        } else if path.is_file() {
            builder
                .append_path_with_name(&path, &rel)
                .with_context(|| format!("adding {}", path.display()))?;
        }
    }
    Ok(())
}

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut dec = flate2::read::GzDecoder::new(bytes);
    std::io::Read::read_to_end(&mut dec, &mut out).context("gunzipping the session log")?;
    Ok(out)
}

fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes).context("gzip")?;
    enc.finish().context("gzip")
}

/// Unix seconds now, for records and events.
pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-suspend-{tag}-{}-{}",
            std::process::id(),
            now_secs() as u64
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A one-shot HTTP server over a real socket: serves each artifact GET from `bodies` keyed by
    /// the artifact kind in the path, closing the connection after every reply.
    fn serve_artifacts(
        bodies: Vec<(&'static str, Vec<u8>)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for _ in 0..bodies.len() {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap() == 0 || header.trim().is_empty() {
                        break;
                    }
                }
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let body = bodies
                    .iter()
                    .find(|(kind, _)| path.contains(kind))
                    .map(|(_, b)| b.clone());
                match body {
                    Some(body) => {
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .unwrap();
                        stream.write_all(&body).unwrap();
                    }
                    None => write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap(),
                }
                stream.flush().unwrap();
                paths.push(path);
            }
            paths
        });
        (addr, handle)
    }

    #[test]
    fn fetch_resume_restores_the_session_log_and_the_workspace_from_the_drop_box() {
        let _guard = crate::test_env_lock();
        let root = temp("fetch-src");
        let state = root.join("state");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(state.join("session.jsonl"), "{\"kind\":\"note\"}\n").unwrap();
        std::fs::write(state.join("admissions.jsonl"), "admitted\n").unwrap();
        git(&workspace, &["init", "-q"]);
        std::fs::write(workspace.join("a.txt"), "one").unwrap();
        git(&workspace, &["add", "-A"]);
        git(
            &workspace,
            &[
                "-c",
                "user.email=c@l",
                "-c",
                "user.name=c",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "one",
            ],
        );
        let head = head_of(&workspace).expect("head");
        let record = ResumeRecord {
            v: ResumeRecord::VERSION,
            run_id: "run-7".into(),
            gate: "approve:run-7:ship".into(),
            head: Some(head.clone()),
            suspended_at: 2.0,
        };
        let snapshot_gz = snapshot(&state, &workspace, &record).expect("snapshot");
        let session_gz = gzip(b"{\"kind\":\"note\"}\n").expect("gzip");

        let (base_url, server) = serve_artifacts(vec![
            (ArtifactKind::RunSession.as_str(), session_gz),
            (ArtifactKind::RunWorkspace.as_str(), snapshot_gz),
        ]);

        let token = root.join("token");
        std::fs::write(&token, "pod-token").unwrap();
        unsafe {
            std::env::set_var(crucible_contract::ENV_INGEST_URL, &base_url);
            std::env::set_var(crucible_contract::ENV_INGEST_TOKEN_PATH, &token);
            std::env::set_var(crucible_contract::ENV_POD_NAME, "crucible-run-7-b");
            std::env::set_var(crucible_contract::ENV_RESUME_OF, "crucible-run-7-a");
        }
        let into = temp("fetch-dst");
        let back = fetch_resume(&into, "workspace").expect("fetch-resume");
        unsafe {
            for k in [
                crucible_contract::ENV_INGEST_URL,
                crucible_contract::ENV_INGEST_TOKEN_PATH,
                crucible_contract::ENV_POD_NAME,
                crucible_contract::ENV_RESUME_OF,
            ] {
                std::env::remove_var(k);
            }
        }

        assert_eq!(back.as_ref(), Some(&record));
        assert_eq!(
            std::fs::read_to_string(into.join("state").join("session.jsonl")).unwrap(),
            "{\"kind\":\"note\"}\n",
            "the session log is restored from its own artifact"
        );
        assert_eq!(
            std::fs::read_to_string(into.join("state").join("admissions.jsonl")).unwrap(),
            "admitted\n"
        );
        assert_eq!(
            head_of(&into.join("workspace")).as_deref(),
            Some(head.as_str())
        );

        let paths = server.join().expect("server");
        assert!(
            paths.iter().all(|p| p.contains("from=crucible-run-7-a")),
            "each fetch names the pod being resumed: {paths:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&into);
    }

    #[test]
    fn a_snapshot_round_trips_state_files_and_the_workspace_history() {
        let root = temp("roundtrip");
        let state = root.join("state");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(state.join("session.jsonl"), "{\"kind\":\"note\"}\n").unwrap();
        std::fs::write(state.join("admissions.jsonl"), "admitted\n").unwrap();
        std::fs::create_dir_all(state.join("files").join("draft")).unwrap();
        std::fs::write(state.join("files").join("draft").join("NOTES.md"), "notes").unwrap();
        git(&workspace, &["init", "-q"]);
        std::fs::write(workspace.join("a.txt"), "one").unwrap();
        git(&workspace, &["add", "-A"]);
        git(
            &workspace,
            &[
                "-c",
                "user.email=c@l",
                "-c",
                "user.name=c",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "one",
            ],
        );
        let head = head_of(&workspace).expect("head");
        let record = ResumeRecord {
            v: ResumeRecord::VERSION,
            run_id: "run-1".into(),
            gate: "approve:run-1:gate".into(),
            head: Some(head.clone()),
            suspended_at: 1.0,
        };
        let gz = snapshot(&state, &workspace, &record).expect("snapshot");
        assert!(
            state.join(RESUME_FILE).exists(),
            "the record is left in state/ for a local resume"
        );

        let other = temp("restored");
        let state2 = other.join("state");
        let workspace2 = other.join("workspace");
        let back = restore(&gz, &state2, &workspace2).expect("restore");
        assert_eq!(back.as_ref(), Some(&record));
        assert_eq!(
            std::fs::read_to_string(state2.join("admissions.jsonl")).unwrap(),
            "admitted\n"
        );
        assert_eq!(
            std::fs::read_to_string(state2.join("files").join("draft").join("NOTES.md")).unwrap(),
            "notes"
        );
        assert!(
            !state2.join("session.jsonl").exists(),
            "the session log travels as its own artifact"
        );
        assert_eq!(head_of(&workspace2).as_deref(), Some(head.as_str()));
        assert_eq!(
            std::fs::read_to_string(workspace2.join("a.txt")).unwrap(),
            "one"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn a_workspace_without_commits_snapshots_without_a_bundle() {
        let root = temp("nobundle");
        let state = root.join("state");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        git(&workspace, &["init", "-q"]);
        let record = ResumeRecord {
            v: ResumeRecord::VERSION,
            run_id: "run-1".into(),
            gate: "g".into(),
            head: None,
            suspended_at: 1.0,
        };
        let gz = snapshot(&state, &workspace, &record).expect("snapshot");
        let other = temp("nobundle-restored");
        let back = restore(&gz, &other.join("state"), &other.join("workspace")).expect("restore");
        assert_eq!(back.map(|r| r.gate), Some("g".to_string()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn a_snapshot_entry_that_escapes_is_refused() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        // `append_data` refuses to write `..`, so the traversal goes straight into the name field:
        // the archive under attack is one an attacker wrote, not one this builder produced.
        let name = b"state/../../evil";
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        builder.append(&header, b"hi".as_slice()).unwrap();
        let tar = builder.into_inner().unwrap();
        let gz = gzip(&tar).unwrap();
        let root = temp("escape");
        assert!(restore(&gz, &root.join("state"), &root.join("workspace")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}

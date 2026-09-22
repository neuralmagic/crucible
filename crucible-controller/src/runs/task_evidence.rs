//! What one task of a run actually did, read back off the evidence the run left behind: the files
//! it captured under a local run's `state/files/<task>/`, and the `task_result` line its session
//! log carries. Behind `GET /api/runs/{run_id}/tasks/{task}/evidence` and
//! `GET /api/runs/{run_id}/log`.
//!
//! Every path here is built from caller-supplied text, so the segments are checked before the join
//! and the resolved directory is checked against the run's own files root afterwards — a symlinked
//! task directory does not get to read the rest of the disk.

use crucible::plan::exec::TaskStatus;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Cap on a captured file served inline. Bigger files list their name and size only.
pub const MAX_INLINE_BYTES: u64 = 256 * 1024;

/// How much of the engine log a run's log response carries, counted from the end.
pub const MAX_LOG_BYTES: u64 = 512 * 1024;

/// How many captured files one task's evidence lists.
const MAX_FILES: usize = 64;

/// One file a task captured into the run's state directory.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedFile {
    pub name: String,
    pub size_bytes: u64,
    /// The file's text, for UTF-8 content under [`MAX_INLINE_BYTES`]; `None` for anything bigger
    /// or not decodable.
    pub content: Option<String>,
}

/// A path segment that cannot leave the directory it is joined onto: no separators, no `.`/`..`,
/// no NUL, not empty. Task names carry brackets (`triage[1027]`) and run ids carry colons, so the
/// rule is about traversal, not about an alphabet.
pub fn safe_segment(seg: &str) -> bool {
    !seg.is_empty() && seg != "." && seg != ".." && !seg.contains(['/', '\\', '\0'])
}

/// The directory a local run unpacks itself into: `<scratch>/local-runs/<sanitized run id>`.
pub fn run_dir(scratch_root: &Path, run_id: &str) -> PathBuf {
    scratch_root
        .join("local-runs")
        .join(crate::model::sanitize_key(run_id))
}

/// Where the supervisor mirrors a local run's engine output.
pub fn engine_log(run_dir: &Path) -> PathBuf {
    run_dir.join("engine.log")
}

/// The root every task's captured files sit under, for one run.
pub(crate) fn files_root(scratch_root: &Path, run_id: &str) -> PathBuf {
    run_dir(scratch_root, run_id)
        .join("pack")
        .join("state")
        .join("files")
}

/// Resolve one task's captured-files directory, or `None` when it does not exist or resolves
/// outside the run's own files root.
fn task_files_dir(scratch_root: &Path, run_id: &str, task: &str) -> Option<PathBuf> {
    if !safe_segment(run_id) || !safe_segment(task) {
        return None;
    }
    let root = files_root(scratch_root, run_id).canonicalize().ok()?;
    let dir = root.join(task).canonicalize().ok()?;
    dir.starts_with(&root).then_some(dir)
}

/// Every file one task captured, name-sorted, each with its size and — for decodable text under
/// the cap — its content. A task that captured nothing, a pod run (whose files never touch this
/// machine), and a traversal attempt all read the same: no files.
pub async fn captured_files(scratch_root: &Path, run_id: &str, task: &str) -> Vec<CapturedFile> {
    let Some(dir) = task_files_dir(scratch_root, run_id, task) else {
        return Vec::new();
    };
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return Vec::new();
    };
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !safe_segment(name) {
            continue;
        }
        // Symlinks are skipped rather than followed: the file the run captured is a regular file,
        // and a link is a way out of the directory.
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() || meta.file_type().is_symlink() {
            continue;
        }
        let size_bytes = meta.len();
        let content = if size_bytes <= MAX_INLINE_BYTES {
            tokio::fs::read(entry.path())
                .await
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
        } else {
            None
        };
        files.push(CapturedFile {
            name: name.to_string(),
            size_bytes,
            content,
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files.truncate(MAX_FILES);
    files
}

/// The tail of `bytes` within [`MAX_LOG_BYTES`], plus whether the head was dropped.
pub fn tail_within_cap(bytes: &[u8]) -> (String, bool) {
    let cap = usize::try_from(MAX_LOG_BYTES).unwrap_or(usize::MAX);
    let truncated = bytes.len() > cap;
    let tail = if truncated {
        // Cut on a line boundary so the first line served is a whole one.
        let from = bytes.len() - cap;
        let start = bytes[from..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(from, |i| from + i + 1);
        &bytes[start..]
    } else {
        bytes
    };
    (String::from_utf8_lossy(tail).into_owned(), truncated)
}

/// The tail of a local run's engine log, plus whether the head was dropped. `None` when the run
/// kept no log.
pub async fn engine_log_tail(scratch_root: &Path, run_id: &str) -> Option<(String, bool)> {
    if !safe_segment(run_id) {
        return None;
    }
    let bytes = tokio::fs::read(engine_log(&run_dir(scratch_root, run_id)))
        .await
        .ok()?;
    Some(tail_within_cap(&bytes))
}

/// The session log a local run published inside its own directory, read when the artifact store
/// holds none for the run.
pub async fn local_session(scratch_root: &Path, run_id: &str) -> Option<String> {
    if !safe_segment(run_id) {
        return None;
    }
    let path = run_dir(scratch_root, run_id)
        .join("pack")
        .join("state")
        .join("session.jsonl");
    tokio::fs::read_to_string(path).await.ok()
}

/// What a run's session log says about one task's last terminal attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTaskResult {
    /// How the attempt ended, decoded through the engine's own type rather than matched as text.
    /// A line whose status the engine does not know is not evidence of a pass, so it reads as
    /// [`TaskStatus::Fail`].
    pub status: TaskStatus,
    /// How many tries the executor made, as the executor counted them.
    pub attempts: Option<i64>,
    /// The payload the task emitted; absent when it emitted none.
    pub payload: Option<Value>,
}

impl SessionTaskResult {
    /// Whether the attempt this describes passed. The one question every caller actually asks.
    pub fn passed(&self) -> bool {
        self.status.passed()
    }
}

/// Read one task's last `task_result` line out of a session log. Lines that are not JSON, not a
/// `task_result`, or not this task's are skipped: a session log is append-only evidence, and one
/// unreadable line is no reason to serve nothing.
///
/// The LAST matching line wins, which is the executor's own last word on the task — including a
/// failing attempt after a passing one, which is why the status has to be carried rather than
/// assumed.
pub fn session_result(session: &str, task: &str) -> Option<SessionTaskResult> {
    let mut found = None;
    for line in session.lines() {
        let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event.get("kind").and_then(Value::as_str) != Some("task_result") {
            continue;
        }
        if event.get("task").and_then(Value::as_str) != Some(task) {
            continue;
        }
        found = Some(SessionTaskResult {
            status: event
                .get("status")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(TaskStatus::Fail),
            attempts: event.get("attempts").and_then(Value::as_i64),
            payload: event.get("output").cloned().filter(|o| !o.is_null()),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fanned-out instance names a real triage run wrote, brackets and all.
    const SESSION: &str = concat!(
        "{\"v\":1,\"kind\":\"plan_admitted\",\"plan_version\":1,\"tasks\":[]}\n",
        "not json at all\n",
        "{\"v\":1,\"kind\":\"task_result\",\"task\":\"triage[1027]\",\"status\":\"fail\",\"attempts\":1,\"output\":null}\n",
        "{\"v\":1,\"kind\":\"task_result\",\"task\":\"triage[1027]\",\"status\":\"pass\",\"attempts\":2,\"output\":{\"severity\":\"low\"}}\n",
        "{\"v\":1,\"kind\":\"task_result\",\"task\":\"roundup\",\"status\":\"pass\",\"attempts\":1,\"output\":{\"triaged\":4}}\n",
    );

    #[test]
    fn the_last_attempt_of_the_named_task_wins() {
        let found = session_result(SESSION, "triage[1027]").expect("the instance reported");
        assert_eq!(found.attempts, Some(2));
        assert_eq!(found.payload, Some(serde_json::json!({"severity": "low"})));
        assert_eq!(
            session_result(SESSION, "roundup").and_then(|r| r.payload),
            Some(serde_json::json!({"triaged": 4})),
        );
        assert_eq!(session_result(SESSION, "scan"), None, "never reported");
    }

    #[test]
    fn a_null_output_is_no_payload() {
        let one = "{\"kind\":\"task_result\",\"task\":\"a\",\"status\":\"pass\",\"attempts\":1,\"output\":null}";
        assert_eq!(
            session_result(one, "a"),
            Some(SessionTaskResult {
                status: TaskStatus::Pass,
                attempts: Some(1),
                payload: None
            })
        );
    }

    /// The bug this closes: the last word on a task was served without its status, so a failing
    /// retry after a passing attempt read as a pass.
    #[test]
    fn a_failing_attempt_after_a_passing_one_does_not_read_as_a_pass() {
        let session = concat!(
            "{\"kind\":\"task_result\",\"task\":\"a\",\"status\":\"pass\",\"attempts\":1,\"output\":{\"ok\":true}}\n",
            "{\"kind\":\"task_result\",\"task\":\"a\",\"status\":\"fail\",\"attempts\":2,\"output\":null}\n",
        );
        let found = session_result(session, "a").expect("reported");
        assert_eq!(found.status, TaskStatus::Fail);
        assert!(!found.passed());
        assert_eq!(found.attempts, Some(2));
        assert_eq!(found.payload, None, "the failing attempt emitted none");
    }

    /// Every non-pass status is carried as itself, and an unknown one is not a pass.
    #[test]
    fn every_status_survives_and_an_unknown_one_fails_closed() {
        for (token, want) in [
            ("pass", TaskStatus::Pass),
            ("fail", TaskStatus::Fail),
            ("transport", TaskStatus::Transport),
            ("skipped", TaskStatus::Skipped),
            ("blocked", TaskStatus::Blocked),
            ("truncated", TaskStatus::Truncated),
            ("something-new", TaskStatus::Fail),
        ] {
            let line = format!(
                "{{\"kind\":\"task_result\",\"task\":\"a\",\"status\":\"{token}\",\"attempts\":1}}"
            );
            let found = session_result(&line, "a").expect("reported");
            assert_eq!(found.status, want, "{token}");
            assert_eq!(found.passed(), token == "pass", "{token}");
        }
        // A line with no status at all is not evidence of a pass either.
        let no_status = "{\"kind\":\"task_result\",\"task\":\"a\",\"attempts\":1}";
        assert!(!session_result(no_status, "a").expect("reported").passed());
    }

    #[test]
    fn traversal_segments_are_refused_and_bracketed_names_are_not() {
        for bad in ["..", ".", "", "a/b", "a\\b", "../../etc"] {
            assert!(!safe_segment(bad), "{bad:?} must not be a path segment");
        }
        for good in ["triage[1027]", "playbook_triage-local_01a0", "REPORT.md"] {
            assert!(safe_segment(good), "{good:?} is an ordinary name");
        }
    }

    #[tokio::test]
    async fn files_are_read_under_the_runs_own_directory_only() {
        let root = tempfile::tempdir().expect("tempdir");
        let scratch = root.path();
        let task_dir = files_root(scratch, "run-1").join("triage[1027]");
        std::fs::create_dir_all(&task_dir).expect("mkdir");
        std::fs::write(task_dir.join("TRIAGE.md"), "# triage\n").expect("write");
        std::fs::write(task_dir.join("blob.bin"), [0xff_u8, 0xfe, 0x00]).expect("write");
        std::fs::create_dir(task_dir.join("nested")).expect("mkdir");

        let files = captured_files(scratch, "run-1", "triage[1027]").await;
        assert_eq!(files.len(), 2, "the directory is not a file: {files:?}");
        assert_eq!(files[0].name, "TRIAGE.md");
        assert_eq!(files[0].content.as_deref(), Some("# triage\n"));
        assert_eq!(files[1].name, "blob.bin");
        assert_eq!(files[1].content, None, "not decodable as text");

        // A traversal in either segment reads nothing, however the join would have resolved.
        assert!(captured_files(scratch, "run-1", "..").await.is_empty());
        assert!(
            captured_files(scratch, "..", "triage[1027]")
                .await
                .is_empty()
        );
        assert!(
            captured_files(scratch, "run-1", "../../../../etc")
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_symlinked_task_directory_reads_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let scratch = root.path();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("SECRET.md"), "not yours\n").expect("write");
        let files = files_root(scratch, "run-1");
        std::fs::create_dir_all(&files).expect("mkdir");
        std::os::unix::fs::symlink(&outside, files.join("escape")).expect("symlink");

        assert!(captured_files(scratch, "run-1", "escape").await.is_empty());
    }

    #[tokio::test]
    async fn a_local_runs_own_session_is_readable_when_the_store_holds_none() {
        let root = tempfile::tempdir().expect("tempdir");
        let scratch = root.path();
        assert_eq!(local_session(scratch, "run-1").await, None);

        let state = run_dir(scratch, "run-1").join("pack").join("state");
        std::fs::create_dir_all(&state).expect("mkdir");
        std::fs::write(state.join("session.jsonl"), SESSION).expect("write");

        let session = local_session(scratch, "run-1").await.expect("a session");
        assert_eq!(
            session_result(&session, "roundup").and_then(|r| r.payload),
            Some(serde_json::json!({"triaged": 4})),
        );
        assert_eq!(local_session(scratch, "..").await, None);
    }

    #[tokio::test]
    async fn the_log_tail_keeps_whole_lines() {
        let root = tempfile::tempdir().expect("tempdir");
        let scratch = root.path();
        assert_eq!(engine_log_tail(scratch, "run-1").await, None, "no log yet");

        let dir = run_dir(scratch, "run-1");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let body = format!("{}\nlast line\n", "x".repeat(MAX_LOG_BYTES as usize));
        std::fs::write(engine_log(&dir), &body).expect("write");

        let (tail, truncated) = engine_log_tail(scratch, "run-1").await.expect("a log");
        assert!(truncated, "the head was dropped");
        assert_eq!(tail, "last line\n", "cut on a line boundary");
    }
}

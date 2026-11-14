//! What one task of a run actually did, read back off the evidence the run left behind: the
//! `task_result` line its session log carries, and where a local run keeps its state. Behind
//! `GET /api/runs/{run_id}/tasks/{task}/evidence` and `GET /api/runs/{run_id}/log`.
//!
//! Every path here is built from caller-supplied text, so the segments are checked before the join
//! and the resolved directory is checked against the run's own files root afterwards — a symlinked
//! task directory does not get to read the rest of the disk.

use crucible_contract::session::TaskStatus;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Cap on a captured file served inline. Bigger files list their name and size only.
pub const MAX_INLINE_BYTES: u64 = 256 * 1024;

/// How much of the engine log one run log response carries.
pub const MAX_LOG_BYTES: u64 = 512 * 1024;

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

/// A local run's state directory: its session log, captured files, and git memory.
pub(crate) fn state_dir(scratch_root: &Path, run_id: &str) -> PathBuf {
    run_dir(scratch_root, run_id).join("pack").join("state")
}

/// The root every task's captured files sit under, for one run.
pub(crate) fn files_root(scratch_root: &Path, run_id: &str) -> PathBuf {
    state_dir(scratch_root, run_id).join("files")
}

/// The one path segment `task`'s captured files live under, as the engine names it: a fan-out
/// instance's key may hold a `/` and is escaped there. `None` when even that is no single segment.
pub fn task_segment(task: &str) -> Option<String> {
    let segment = crucible::plan::ir::TaskName(task.to_owned())
        .dir_name()
        .into_owned();
    safe_segment(&segment).then_some(segment)
}

/// One window of a log within [`MAX_LOG_BYTES`], cut on line boundaries.
#[derive(Debug, PartialEq, Eq)]
pub struct LogWindow {
    pub text: String,
    /// Byte offset of the window's first byte in the whole log.
    pub start: u64,
    /// Byte offset to ask for next, when the log continues past this window.
    pub next: Option<u64>,
    pub total_bytes: u64,
}

impl LogWindow {
    /// True when the window is not the whole log.
    pub fn truncated(&self) -> bool {
        self.start > 0 || self.next.is_some()
    }
}

/// The window of `bytes` starting at `from`, or the tail when `from` is `None`. A window that
/// stops short of the end stops after its last whole line, or inside a line longer than the cap on
/// a character boundary, and the next one starts at that byte, so paging from 0 by `next` reads
/// every byte exactly once.
pub fn log_window(bytes: &[u8], from: Option<u64>) -> LogWindow {
    let cap = usize::try_from(MAX_LOG_BYTES).unwrap_or(usize::MAX);
    let len = bytes.len();
    let (start, end) = match from {
        None if len <= cap => (0, len),
        None => {
            let from = len - cap;
            let start = bytes[from..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(from, |i| from + i + 1);
            (if start == len { from } else { start }, len)
        }
        Some(from) => {
            let start = usize::try_from(from).unwrap_or(usize::MAX).min(len);
            let end = start.saturating_add(cap).min(len);
            let end = if end == len {
                end
            } else {
                bytes[start..end]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or_else(|| char_boundary(bytes, start, end), |i| start + i + 1)
            };
            (start, end)
        }
    };
    LogWindow {
        text: String::from_utf8_lossy(&bytes[start..end]).into_owned(),
        start: offset(start),
        next: (end < len).then(|| offset(end)),
        total_bytes: offset(len),
    }
}

/// The last UTF-8 character boundary in `(start, end]`, or `end` when the bytes there are not
/// UTF-8.
fn char_boundary(bytes: &[u8], start: usize, end: usize) -> usize {
    (start + 1..=end)
        .rev()
        .take(4)
        .find(|at| bytes.get(*at).is_none_or(|b| b & 0xC0 != 0x80))
        .unwrap_or(end)
}

fn offset(at: usize) -> u64 {
    u64::try_from(at).unwrap_or(u64::MAX)
}

/// A local run's engine log. `None` when the run kept no log.
pub async fn engine_log_bytes(scratch_root: &Path, run_id: &str) -> Option<Vec<u8>> {
    if !safe_segment(run_id) {
        return None;
    }
    tokio::fs::read(engine_log(&run_dir(scratch_root, run_id)))
        .await
        .ok()
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

/// The session log a run has: the store first, then the log a local run published in its own
/// directory, because a run whose session never reached the store still has its evidence on this
/// disk.
pub async fn run_session(
    pool: &sqlx::PgPool,
    scratch_root: &Path,
    run_id: &str,
) -> anyhow::Result<Option<String>> {
    match crate::runs::blob_store::get_run_session(pool, run_id).await? {
        Some(stored) => Ok(Some(stored)),
        None => Ok(local_session(scratch_root, run_id).await),
    }
}

/// How many instances a mapped task was spread over, and `None` when the session cannot say.
///
/// The executor folds a finished fan-out into one result for the node itself, counting the
/// instances it made after the rules that can refuse a fan-out outright (a producer that did not
/// pass, a list over `max_fanout`, an item that cannot name an instance). That count is the
/// answer whenever the node has settled, and a node that settled without one was refused, so its
/// width is nothing rather than its producer's list length.
///
/// Until the node settles the producer's own `producer.field` list is the only count there is.
pub fn fanout_width(session: &str, task: &str, over: &str) -> Option<usize> {
    if let Some(folded) = session_result(session, task) {
        let instances = folded.payload?.get("instances")?.as_u64()?;
        return usize::try_from(instances).ok();
    }
    let (producer, field) = over.rsplit_once('.')?;
    let emitted = session_result(session, producer)?;
    if !emitted.passed() {
        return None;
    }
    Some(emitted.payload?.get(field)?.as_array()?.len())
}

#[cfg(test)]
mod tests {
    use crate::runs::task_evidence::*;

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

    /// A raw instance name may hold a `/` in its key, which the engine escaped where it formed the
    /// directory; a name that climbs anywhere else still has no segment.
    #[test]
    fn a_task_segment_escapes_an_instance_key_and_refuses_a_climbing_name() {
        assert_eq!(
            task_segment("review[llm-d/llm-d-router]").as_deref(),
            Some("review[llm-d%2Fllm-d-router]")
        );
        assert_eq!(
            task_segment("review[../..]").as_deref(),
            Some("review[%2E.%2F..]")
        );
        assert_eq!(
            task_segment("triage[1027]").as_deref(),
            Some("triage[1027]")
        );
        for bad in ["..", ".", "", "a/b", "../../etc", "a/b[c]"] {
            assert_eq!(task_segment(bad), None, "{bad:?}");
        }
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

    #[test]
    fn the_log_tail_keeps_whole_lines() {
        let body = format!("{}\nlast line\n", "x".repeat(MAX_LOG_BYTES as usize));
        let tail = log_window(body.as_bytes(), None);
        assert!(tail.truncated(), "the head was dropped");
        assert_eq!(tail.text, "last line\n", "cut on a line boundary");
        assert_eq!(tail.start, offset(body.len() - 10));
        assert_eq!(tail.next, None);
    }

    fn paged(body: &[u8]) -> (Vec<u8>, usize) {
        let mut read = Vec::new();
        let mut from = Some(0);
        let mut windows = 0;
        while let Some(at) = from {
            let window = log_window(body, Some(at));
            assert!(window.text.len() <= usize::try_from(MAX_LOG_BYTES).unwrap());
            assert_eq!(window.start, at);
            assert_eq!(window.total_bytes, offset(body.len()));
            assert_eq!(window.truncated(), body.len() > window.text.len());
            if window.next.is_some() {
                assert!(
                    !window.text.is_empty(),
                    "a window short of the end makes progress"
                );
            }
            read.extend_from_slice(window.text.as_bytes());
            from = window.next;
            windows += 1;
        }
        (read, windows)
    }

    #[test]
    fn paging_from_the_start_reads_every_line_once() {
        let line = format!("{}\n", "y".repeat(999));
        let mut body = String::new();
        for i in 0..1600 {
            body.push_str(&format!("{i:05} {line}"));
        }
        let (read, windows) = paged(body.as_bytes());
        assert_eq!(read, body.as_bytes());
        assert!(windows > 1, "the log spans several windows");
        let first = log_window(body.as_bytes(), Some(0));
        assert!(first.text.ends_with('\n'), "a window ends on a whole line");

        let mid = log_window(body.as_bytes(), Some(3));
        assert_eq!(
            (mid.start, &mid.text[..3]),
            (3, "00 "),
            "an offset is served as asked"
        );
        let past = log_window(body.as_bytes(), Some(u64::MAX));
        assert_eq!((past.text.as_str(), past.next), ("", None));
    }

    #[test]
    fn a_line_longer_than_one_response_pages_back_whole() {
        let cap = usize::try_from(MAX_LOG_BYTES).unwrap();
        let long = "é".repeat(cap);
        let body = format!("head\n{long}\ntail\n");
        let (read, windows) = paged(body.as_bytes());
        assert_eq!(
            String::from_utf8(read).unwrap(),
            body,
            "no byte lost or replaced"
        );
        assert!(windows >= 3, "the long line spans windows: {windows}");

        let unbroken = "x".repeat(3 * cap);
        let (read, windows) = paged(unbroken.as_bytes());
        assert_eq!(read, unbroken.as_bytes());
        assert_eq!(windows, 3);
    }

    #[tokio::test]
    async fn a_local_run_without_a_log_has_none() {
        let root = tempfile::tempdir().expect("tempdir");
        assert_eq!(engine_log_bytes(root.path(), "run-1").await, None);
        assert_eq!(engine_log_bytes(root.path(), "..").await, None);
    }

    /// The fold the executor writes for a mapped node counts the instances it made, so that is
    /// the width even when the producer's list says something else.
    #[test]
    fn a_settled_fan_out_is_counted_off_its_own_fold() {
        let session = concat!(
            "{\"kind\":\"task_result\",\"task\":\"scan\",\"status\":\"pass\",\"output\":{\"issues\":[\"a\",\"b\",\"c\"]}}\n",
            "{\"kind\":\"task_result\",\"task\":\"triage\",\"status\":\"fail\",\"output\":{\"instances\":20,\"passed\":4,\"failed\":16}}\n",
        );
        assert_eq!(fanout_width(session, "triage", "scan.issues"), Some(20));
    }

    /// A fan-out the executor refused settles the node with no fold, and a refused fan-out is no
    /// instances at all: the producer's list is not evidence that any of it ran.
    #[test]
    fn a_refused_fan_out_reports_no_width() {
        let session = concat!(
            "{\"kind\":\"task_result\",\"task\":\"scan\",\"status\":\"pass\",\"output\":{\"issues\":[\"a\",\"b\"]}}\n",
            "{\"kind\":\"task_result\",\"task\":\"triage\",\"status\":\"fail\",\"output\":null,\"note\":\"scan.issues has 2 items; max_fanout is 1\"}\n",
        );
        assert_eq!(fanout_width(session, "triage", "scan.issues"), None);
    }

    /// Before the node settles, the producer's list is the count — and only a producer that
    /// passed is one the executor would map over.
    #[test]
    fn a_running_fan_out_is_counted_off_its_producer() {
        let session = concat!(
            "{\"kind\":\"task_result\",\"task\":\"scan\",\"status\":\"pass\",\"output\":{\"issues\":[\"a\",\"b\",\"c\"],\"count\":3}}\n",
            "{\"kind\":\"task_result\",\"task\":\"flaky\",\"status\":\"fail\",\"output\":{\"issues\":[\"a\"]}}\n",
            "{\"kind\":\"task_result\",\"task\":\"quiet\",\"status\":\"pass\",\"output\":null}\n",
            "{\"kind\":\"task_result\",\"task\":\"with.dots\",\"status\":\"pass\",\"output\":{\"issues\":[\"a\"]}}\n",
        );
        let width = |over: &str| fanout_width(session, "triage", over);
        assert_eq!(width("scan.issues"), Some(3));
        assert_eq!(width("with.dots.issues"), Some(1), "split on the last dot");
        assert_eq!(width("flaky.issues"), None, "the producer failed");
        assert_eq!(width("scan.count"), None, "not a list");
        assert_eq!(width("scan.missing"), None, "no such field");
        assert_eq!(width("quiet.issues"), None, "no payload");
        assert_eq!(width("absent.issues"), None, "never reported");
        assert_eq!(width("scan"), None, "not producer.field");
    }

    /// Zero items is a width, not an absence.
    #[test]
    fn a_fan_out_over_nothing_is_zero_wide() {
        let empty = "{\"kind\":\"task_result\",\"task\":\"scan\",\"status\":\"pass\",\"output\":{\"issues\":[]}}";
        assert_eq!(fanout_width(empty, "triage", "scan.issues"), Some(0));
        let folded = "{\"kind\":\"task_result\",\"task\":\"triage\",\"status\":\"pass\",\"output\":{\"instances\":0,\"passed\":0,\"failed\":0}}";
        assert_eq!(fanout_width(folded, "triage", "scan.issues"), Some(0));
    }
}

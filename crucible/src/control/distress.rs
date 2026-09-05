//! The engine half of the broker's `distress` tool: two handoff files under `forge::storage_root()`
//! (the same channel as `turn-token`/`turn-traceparent`).
//!
//! ```text
//!   <storage>/distress             the park marker; while it exists the loop is suspended
//!   <storage>/distress-notes.jsonl info/warn notes, consumed into the next decided row
//! ```
//!
//! The marker is read, never deleted: clearing the file is the operator's resume grant, so the
//! engine deleting it would grant itself the thing it is waiting for. Notes are the opposite,
//! consumed on read like `CANDIDATE.md`, so a note lands on exactly one row.

use serde::Deserialize;
use std::path::PathBuf;
use std::time::SystemTime;

/// The approval handle (and trace id) a distress park opens its wait under. A resume recognizes it
/// and refuses to re-park: no rescope will ever arrive for a distress.
pub(crate) const HANDLE: &str = "distress";

/// The broker's park marker. `turn_token` is written too but the engine has no use for it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Marker {
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub ts_ms: u64,
}

fn marker_path() -> PathBuf {
    forge::storage_root().join("distress")
}

fn notes_path() -> PathBuf {
    forge::storage_root().join("distress-notes.jsonl")
}

/// The park marker, if the broker wrote one. `None` = no distress (also the degraded answer when
/// the storage dir doesn't exist, e.g. a local run); `Some(Err)` = the file is there but unreadable
/// or malformed, which the caller notes without parking (never wedge a paid run on a bad byte).
pub(crate) fn read_marker() -> Option<Result<Marker, String>> {
    let path = marker_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err(format!("reading {}: {e}", path.display()))),
    };
    Some(serde_json::from_str(&raw).map_err(|e| format!("parsing {}: {e}", path.display())))
}

/// Modification time of the marker, the key a malformed-marker note dedupes on: a rewritten
/// (still bad) marker is a new complaint, the same bytes are not.
pub(crate) fn marker_mtime() -> Option<SystemTime> {
    std::fs::metadata(marker_path())
        .and_then(|m| m.modified())
        .ok()
}

/// Take the pending info/warn notes as `(severity, reason)`, deleting the file. Torn or malformed
/// lines are skipped rather than failing the batch: the writer appends without locking, so a line
/// can be half-written when the engine reads, and a lost status note must not cost an iteration.
pub(crate) fn drain_notes() -> Vec<(String, String)> {
    #[derive(Deserialize)]
    struct Note {
        severity: String,
        reason: String,
    }
    let path = notes_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let _ = std::fs::remove_file(&path);
    raw.lines()
        .filter_map(|l| serde_json::from_str::<Note>(l).ok())
        .map(|n| (n.severity, n.reason))
        .collect()
}

/// Scratch storage root for any test that exercises the handoff files (here and in the loop
/// driver). `FORGE_STORAGE_ROOT` is process-global, so the lock is part of the contract.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    pub(crate) struct Root {
        pub dir: PathBuf,
    }

    impl Root {
        /// The caller must hold [`env_lock`] (directly, or via a run_loop fixture) for the whole
        /// lifetime of the returned `Root`, and must drop the `Root` first so the env var is gone
        /// before the next test takes the lock.
        pub(crate) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("crucible-distress-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: the ENV mutex scopes the mutation to one test at a time.
            unsafe { std::env::set_var("FORGE_STORAGE_ROOT", &dir) };
            Self { dir }
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            unsafe { std::env::remove_var("FORGE_STORAGE_ROOT") };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::distress::testing::Root;
    use crucible::test_support::env_lock;

    #[test]
    fn a_written_marker_round_trips_and_survives_the_read() {
        let _g = env_lock();
        let root = Root::new("engine-marker");
        std::fs::write(
            root.dir.join("distress"),
            r#"{"reason":"torch skew","evidence":["log-3"],"ts_ms":1234,"turn_token":"t-1"}"#,
        )
        .unwrap();
        let m = read_marker().expect("marker present").expect("well-formed");
        assert_eq!(m.reason, "torch skew");
        assert_eq!(m.evidence, vec!["log-3".to_string()]);
        assert_eq!(m.ts_ms, 1234);
        assert!(
            root.dir.join("distress").exists(),
            "reading must not clear the marker: the operator's rm is the grant"
        );
        assert!(marker_mtime().is_some());
    }

    #[test]
    fn a_missing_marker_and_a_missing_root_both_read_as_none() {
        let _g = env_lock();
        let root = Root::new("engine-absent");
        assert!(read_marker().is_none());
        assert!(marker_mtime().is_none());
        std::fs::remove_dir_all(&root.dir).unwrap();
        assert!(read_marker().is_none(), "no storage dir is not a distress");
    }

    #[test]
    fn a_malformed_marker_reads_as_an_error_and_is_left_in_place() {
        let _g = env_lock();
        let root = Root::new("engine-malformed");
        std::fs::write(root.dir.join("distress"), "{not json").unwrap();
        let err = read_marker().expect("present").expect_err("malformed");
        assert!(err.contains("parsing"), "{err}");
        assert!(
            root.dir.join("distress").exists(),
            "a bad marker is operator evidence, not garbage to collect"
        );
    }

    #[test]
    fn notes_drain_once_and_tolerate_torn_lines() {
        let _g = env_lock();
        let root = Root::new("engine-notes");
        std::fs::write(
            root.dir.join("distress-notes.jsonl"),
            "{\"severity\":\"info\",\"reason\":\"cache warm\",\"ts_ms\":1}\n\
             {\"severity\":\"war\n\
             {\"severity\":\"warn\",\"reason\":\"flaky node\",\"ts_ms\":2}\n",
        )
        .unwrap();
        let notes = drain_notes();
        assert_eq!(
            notes,
            vec![
                ("info".to_string(), "cache warm".to_string()),
                ("warn".to_string(), "flaky node".to_string()),
            ]
        );
        assert!(!root.dir.join("distress-notes.jsonl").exists());
        assert!(drain_notes().is_empty(), "consumed on read");
    }
}

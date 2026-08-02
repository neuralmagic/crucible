//! Durable logical agent sessions.
//!
//! The ledger stores only a logical name, an opaque harness continuation id, and a turn count.
//! It never copies private reasoning or a provider transcript into Crucible's run log. World
//! rollback and agent learning therefore have independent lifetimes: a discarded candidate can
//! restore the checkout while the next turn resumes the solver that learned why it failed.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const LEDGER_FILE: &str = "agent-sessions.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    provider_id: String,
    completed_turns: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    sessions: BTreeMap<String, Entry>,
}

/// One prepared turn. `provider_id` is deliberately opaque outside the harness boundary.
#[derive(Clone, Debug)]
pub(crate) struct SessionTurn {
    pub logical_name: String,
    pub provider_id: String,
    pub completed_turns: u32,
}

impl SessionTurn {
    pub(crate) fn is_resume(&self) -> bool {
        self.completed_turns > 0
    }

    pub(crate) fn action(&self) -> &'static str {
        if self.is_resume() {
            "resumed"
        } else {
            "started"
        }
    }
}

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn ledger_path(state: &Path) -> std::path::PathBuf {
    state.join(LEDGER_FILE)
}

fn read(state: &Path) -> Result<Ledger> {
    let path = ledger_path(state);
    match std::fs::read_to_string(&path) {
        Ok(body) => serde_json::from_str(&body)
            .with_context(|| format!("parsing agent session ledger {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(e) => {
            Err(e).with_context(|| format!("reading agent session ledger {}", path.display()))
        }
    }
}

/// Resolve a logical session without mutating the ledger. A new continuation id is committed only
/// after a successful turn, so a failed spawn cannot poison every later turn with a missing resume.
pub(crate) fn prepare(state: &Path, logical_name: &str) -> Result<SessionTurn> {
    let _guard = lock().lock().unwrap_or_else(|e| e.into_inner());
    let ledger = read(state)?;
    let entry = ledger.sessions.get(logical_name);
    Ok(SessionTurn {
        logical_name: logical_name.to_string(),
        provider_id: entry
            .map(|entry| entry.provider_id.clone())
            .unwrap_or_else(|| Uuid::now_v7().to_string()),
        completed_turns: entry.map_or(0, |entry| entry.completed_turns),
    })
}

/// Advance the opaque continuation cursor after a successful harness turn. The private 0600 file
/// is replaced atomically; it is runtime state, not part of the public session event stream.
pub(crate) fn commit(state: &Path, turn: &SessionTurn) -> Result<()> {
    let _guard = lock().lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(state)
        .with_context(|| format!("creating state directory {}", state.display()))?;
    let mut ledger = read(state)?;
    ledger.sessions.insert(
        turn.logical_name.clone(),
        Entry {
            provider_id: turn.provider_id.clone(),
            completed_turns: turn.completed_turns.saturating_add(1),
        },
    );
    let path = ledger_path(state);
    let tmp = state.join(format!(".{LEDGER_FILE}.tmp"));
    let body = serde_json::to_vec_pretty(&ledger)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .with_context(|| format!("creating agent session ledger temp file {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&body)?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("installing agent session ledger {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_only_opaque_continuation_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let first = prepare(dir.path(), "solver").unwrap();
        assert!(!first.is_resume());
        assert!(!ledger_path(dir.path()).exists());
        commit(dir.path(), &first).unwrap();

        let resumed = prepare(dir.path(), "solver").unwrap();
        assert!(resumed.is_resume());
        assert_eq!(resumed.provider_id, first.provider_id);
        assert_eq!(resumed.completed_turns, 1);
        let body = std::fs::read_to_string(ledger_path(dir.path())).unwrap();
        assert!(!body.contains("reasoning"));
        assert!(!body.contains("prompt"));
        assert!(!body.contains("transcript"));
    }
}

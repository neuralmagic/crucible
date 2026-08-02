//! Durable logical agent sessions. The ledger stores only a logical name, opaque continuation id,
//! and turn count; provider transcripts remain private. World rollback and agent context have
//! independent lifetimes. The cursor advances after any turn without an agent or transport error,
//! including one graded as a failure for omitting its result.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::session::SessionAction;

const LEDGER_FILE: &str = "agent-sessions.json";
const LOCK_FILE: &str = ".agent-sessions.json.lock";

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

    pub(crate) fn action(&self) -> SessionAction {
        if self.is_resume() {
            SessionAction::Resumed
        } else {
            SessionAction::Started
        }
    }
}

/// flock(2) on a sidecar file: `commit` is a read/modify/write and several crucible processes can
/// share one state dir, which an in-process mutex would not cover. The lock is not the ledger
/// itself, whose inode is replaced by every rename.
fn lock(state: &Path) -> Result<Flock<std::fs::File>> {
    let path = state.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening agent session lock {}", path.display()))?;
    Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, errno)| errno)
        .with_context(|| format!("locking agent session ledger via {}", path.display()))
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

/// Resolve a session without mutating its ledger. Unlocked: `commit` installs the ledger by
/// rename, so a reader either sees the old file or the new one.
pub(crate) fn prepare(state: &Path, logical_name: &str) -> Result<SessionTurn> {
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

/// Atomically advance the private cursor after successful transport.
pub(crate) fn commit(state: &Path, turn: &SessionTurn) -> Result<()> {
    std::fs::create_dir_all(state)
        .with_context(|| format!("creating state directory {}", state.display()))?;
    let _guard = lock(state)?;
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

/// `Err` carries the message every call site reports as the turn's error.
pub(crate) fn prepare_named(
    state: &Path,
    session: Option<&str>,
) -> std::result::Result<Option<SessionTurn>, String> {
    match session.map(|name| prepare(state, name)) {
        Some(Ok(turn)) => Ok(Some(turn)),
        Some(Err(e)) => Err(format!("preparing agent session failed: {e:#}")),
        None => Ok(None),
    }
}

/// Advance the cursor only for a turn that transported cleanly; `Some` is the failure message.
pub(crate) fn commit_if_ok(
    state: &Path,
    prepared: Option<&SessionTurn>,
    transported: bool,
) -> Option<String> {
    let turn = prepared.filter(|_| transported)?;
    commit(state, turn)
        .err()
        .map(|e| format!("committing agent session failed: {e:#}"))
}

/// A resumed turn gets the follow-up prompt when the caller has one.
pub(crate) fn effective_prompt<'a>(
    prepared: Option<&SessionTurn>,
    prompt: &'a str,
    resume_prompt: Option<&'a str>,
) -> &'a str {
    match prepared {
        Some(turn) if turn.is_resume() => resume_prompt.unwrap_or(prompt),
        _ => prompt,
    }
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

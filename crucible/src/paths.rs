//! The paths a manifest run anchors off.

use std::path::{Path, PathBuf};

/// Runtime paths for a manifest run. Everything anchors off the manifest dir + an explicit
/// state dir, never the binary's install location (contract §2): a target repo is
/// self-describing, drop a `crucible.toml` at its root and run `crucible` inside it.
#[derive(Clone)]
pub struct Paths {
    pub workspace: PathBuf,
    /// Toolbox source dir (`[agent].toolbox_dir`, manifest-relative); its subdirs are copied
    /// into `<workspace>/.claude/skills` each run. `None` when the manifest sets no toolbox.
    pub skills: Option<PathBuf>,
    pub steer: PathBuf,
    /// Cross-process state dir (gitignored): the session log + control file live here.
    pub state: PathBuf,
    /// Append-only NDJSON event log the headless loop emits for external tailers.
    pub session_log: PathBuf,
    /// Cross-process stop signal written by the `stop` tool.
    pub control: PathBuf,
    /// Escalation marker the agent's `escalate` tool writes in its workspace; the loop detects it
    /// after a turn, restores the world, and halts for human review.
    pub escalation: PathBuf,
    /// Pending-provisioning marker the agent writes when it has an open approval to wait on; the loop
    /// detects it after a turn and parks or continues per its `mode`.
    pub provisioning: PathBuf,
    /// Append-only NDJSON record of every external input, authoritative over the session
    /// log for what an operator asked for; a resume replays it.
    pub admissions: PathBuf,
}

impl Paths {
    pub fn for_manifest(
        workspace: PathBuf,
        state: PathBuf,
        manifest_dir: &Path,
        skills: Option<PathBuf>,
    ) -> Self {
        let escalation = workspace.join("ESCALATION.json");
        let provisioning = workspace.join("PROVISIONING_PENDING.json");
        Self {
            workspace,
            skills,
            steer: manifest_dir.join("STEER.md"),
            session_log: state.join("session.jsonl"),
            control: state.join("control.json"),
            admissions: state.join("admissions.jsonl"),
            escalation,
            provisioning,
            state,
        }
    }

    /// Paths for an isolated task worktree: everything (state, steer, markers) lives inside
    /// the clone so a task cannot touch the parent run's state.
    pub fn for_worktree(worktree: PathBuf, skills: Option<PathBuf>) -> Self {
        Self {
            skills,
            steer: worktree.join("STEER.md"),
            state: worktree.join("state"),
            session_log: worktree.join("state/session.jsonl"),
            control: worktree.join("state/control.json"),
            // An isolated task worktree takes no external input; the path exists only so
            // `Paths` stays one shape.
            admissions: worktree.join("state/admissions.jsonl"),
            escalation: worktree.join("ESCALATION.json"),
            provisioning: worktree.join("PROVISIONING_PENDING.json"),
            workspace: worktree,
        }
    }

    /// The run's buildit socket: in its state dir, building from its workspace.
    pub fn buildit_socket(&self) -> crate::openshell::buildit::RunSocket {
        crate::openshell::buildit::RunSocket {
            path: self.state.join("buildit.sock"),
            workdir: self.workspace.clone(),
        }
    }
}

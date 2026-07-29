//! The any-repo [`World`] batteries: reversibility expressed as git memory plus optional
//! domain commands.
//!
//! - [`GitWorld`]: the 80% case: reversibility *is* git. No domain code; the kept-commit
//!   chain is the memory. The litmus `examples/counter` runs on this.
//! - [`CommandWorld`]: git memory PLUS `snapshot_cmd`/`restore_cmd` for external state a
//!   commit can't capture (a live deploy, a cluster). The token it stores is the composite
//!   `"<git-sha>\t<domain-token>"`; the engine treats the whole thing as opaque, this module
//!   owns the framing, and neither `run_loop` nor the domain scripts see the other half.

use crate::crucible::World;
use anyhow::{Context, Result, bail};
use crucible_vcs::git_memory;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run a domain command via `sh -c` in the workspace, returning its trimmed stdout. `token`,
/// when present (restore), is passed as the `CRUCIBLE_TOKEN` env var, the opaque snapshot
/// token round-trips through the environment, so no shell-quoting of multi-line payloads.
fn run_cmd(workspace: &Path, cmd: &str, token: Option<&str>) -> Result<String> {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd).current_dir(workspace);
    if let Some(t) = token {
        c.env("CRUCIBLE_TOKEN", t);
    }
    let out = c
        .output()
        .with_context(|| format!("exec `{cmd}` in {}", workspace.display()))?;
    if !out.status.success() {
        bail!(
            "command `{cmd}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The opaque snapshot token is `"<git-sha>\t<domain-token>"`. These keep the framing in one
/// place; the domain half may itself be empty (GitWorld) or base64 (a multi-line capture).
fn join_token(git: &str, domain: &str) -> String {
    format!("{git}\t{domain}")
}
fn split_token(snap: &str) -> (&str, &str) {
    match snap.split_once('\t') {
        Some((git, domain)) => (git, domain),
        None => (snap, ""),
    }
}

/// Reversibility *is* git: snapshot commits, restore resets + cleans. The any-repo default.
pub struct GitWorld {
    pub workspace: PathBuf,
}

impl World for GitWorld {
    fn snapshot(&self, label: &str) -> Result<String> {
        git_memory::snapshot(&self.workspace, label)
    }
    fn restore(&self, snap: &str) -> Result<()> {
        git_memory::restore(&self.workspace, snap)
    }
    fn commit_sha(&self, snap: &str) -> Option<String> {
        Some(snap.to_string())
    }
    fn staged_diff(&self) -> (String, String) {
        git_memory::staged_diff(&self.workspace)
    }
}

/// Git memory + domain `snapshot_cmd`/`restore_cmd` for external state. With both omitted it
/// behaves like [`GitWorld`]; a live deployment (image + configmap) is the canonical user.
pub struct CommandWorld {
    pub workspace: PathBuf,
    pub apply_cmd: Option<String>,
    pub snapshot_cmd: Option<String>,
    pub restore_cmd: Option<String>,
}

impl World for CommandWorld {
    fn apply(&self) -> Result<()> {
        if let Some(cmd) = &self.apply_cmd {
            run_cmd(&self.workspace, cmd, None)?;
        }
        Ok(())
    }
    fn snapshot(&self, label: &str) -> Result<String> {
        let git = git_memory::snapshot(&self.workspace, label)?;
        let domain = match &self.snapshot_cmd {
            Some(cmd) => run_cmd(&self.workspace, cmd, None)?
                .lines()
                .next_back()
                .unwrap_or("")
                .to_string(),
            None => String::new(),
        };
        Ok(join_token(&git, &domain))
    }
    fn restore(&self, snap: &str) -> Result<()> {
        let (git, domain) = split_token(snap);
        git_memory::restore(&self.workspace, git)?;
        if let Some(cmd) = &self.restore_cmd {
            run_cmd(&self.workspace, cmd, Some(domain))?;
        }
        Ok(())
    }
    fn commit_sha(&self, snap: &str) -> Option<String> {
        Some(split_token(snap).0.to_string())
    }
    fn staged_diff(&self) -> (String, String) {
        git_memory::staged_diff(&self.workspace)
    }
}

/// The bundled snapshot token for a [`CompositeWorld`]. Serialized to JSON so it nests the
/// per-component git shas and the combined domain token unambiguously (a component token may itself
/// contain the `\t` that [`CommandWorld`] uses). The engine still treats the whole string as opaque.
#[derive(Serialize, Deserialize)]
struct CompositeToken {
    /// (component name, its git sha), git memory is kept per component.
    components: Vec<(String, String)>,
    /// The combined `snapshot_cmd` token for the assembled system under test (empty when none).
    domain: String,
}

/// A composite world: git memory across N component workspaces PLUS one combined external
/// apply/snapshot/restore for the *assembled* system under test (e.g. a router in front of model replicas). A kept
/// iteration is therefore a tuple of per-component commits, the multi-workspace half of the
/// composite-domain run identity. The combined deployment commands run in `base_dir` (the composite manifest
/// dir), not in any one component workspace, because they act on the assembled system.
pub struct CompositeWorld {
    /// (component name, its workspace). Order is stable; names are unique (manifest-enforced).
    pub components: Vec<(String, PathBuf)>,
    /// Where the combined deployment commands run (the composite manifest dir).
    pub base_dir: PathBuf,
    pub apply_cmd: Option<String>,
    pub snapshot_cmd: Option<String>,
    pub restore_cmd: Option<String>,
}

impl World for CompositeWorld {
    fn apply(&self) -> Result<()> {
        if let Some(cmd) = &self.apply_cmd {
            run_cmd(&self.base_dir, cmd, None)?;
        }
        Ok(())
    }

    fn snapshot(&self, label: &str) -> Result<String> {
        // Commit each component's overlay; collect (name, sha).
        let mut shas = Vec::with_capacity(self.components.len());
        for (name, ws) in &self.components {
            let sha = git_memory::snapshot(ws, label)
                .with_context(|| format!("snapshot component `{name}`"))?;
            shas.push((name.clone(), sha));
        }
        // One combined external snapshot for the assembled system under test (last stdout line = the token).
        let domain = match &self.snapshot_cmd {
            Some(cmd) => run_cmd(&self.base_dir, cmd, None)?
                .lines()
                .next_back()
                .unwrap_or("")
                .to_string(),
            None => String::new(),
        };
        let token = CompositeToken {
            components: shas,
            domain,
        };
        serde_json::to_string(&token).context("encode composite snapshot token")
    }

    fn restore(&self, snap: &str) -> Result<()> {
        let token: CompositeToken =
            serde_json::from_str(snap).context("decode composite snapshot token")?;
        for (name, sha) in &token.components {
            let ws = self
                .components
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, ws)| ws)
                .with_context(|| format!("restore: unknown component `{name}` in token"))?;
            git_memory::restore(ws, sha).with_context(|| format!("restore component `{name}`"))?;
        }
        if let Some(cmd) = &self.restore_cmd {
            run_cmd(&self.base_dir, cmd, Some(&token.domain))?;
        }
        Ok(())
    }

    /// Diff EVERY component repo and combine, the base dir isn't a git repo, so a single top-level
    /// diff would miss the agent's per-component edits entirely (the empty-diff bug). Each component's
    /// hunk is prefixed so the combined diff (and the per-component shortstat) stays attributable.
    fn staged_diff(&self) -> (String, String) {
        let mut diff = String::new();
        let mut stats = Vec::new();
        for (name, ws) in &self.components {
            let (d, s) = git_memory::staged_diff(ws);
            // Only components the agent actually touched contribute, the shortstat is non-empty
            // ("0 files changed…") even with no edits, so gate on the DIFF, not the stat.
            if !d.is_empty() {
                diff.push_str(&format!("=== component: {name} ===\n{d}\n"));
                stats.push(format!("{name}: {s}"));
            }
        }
        (diff, stats.join("; "))
    }

    // commit_sha returns None: a composite has one sha PER component, which the single-sha trait method
    // can't express. The publish layer reads the per-component shas via `publish_components` instead.

    /// The per-component publish targets (multi-fork): zip each component's workspace with the base
    /// sha from the pristine baseline token and the head sha from the kept-candidate token. A
    /// component whose base == head (the agent never touched it) is dropped, it has no PR to open.
    fn publish_components(
        &self,
        base_snap: &str,
        best_snap: &str,
    ) -> Option<Vec<crate::crucible::PublishComponent>> {
        let base: CompositeToken = serde_json::from_str(base_snap).ok()?;
        let best: CompositeToken = serde_json::from_str(best_snap).ok()?;
        let mut out = Vec::new();
        for (name, ws) in &self.components {
            let base_sha = base
                .components
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| s);
            let head_sha = best
                .components
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| s);
            if let (Some(base_sha), Some(head_sha)) = (base_sha, head_sha)
                && base_sha != head_sha
            {
                out.push(crate::crucible::PublishComponent {
                    name: name.clone(),
                    workspace: ws.clone(),
                    base_sha: base_sha.clone(),
                    head_sha: head_sha.clone(),
                });
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips_through_join_split() {
        assert_eq!(split_token(&join_token("abc123", "tok")), ("abc123", "tok"));
        // GitWorld-style (no domain half) and a bare sha both split cleanly.
        assert_eq!(split_token(&join_token("sha", "")), ("sha", ""));
        assert_eq!(split_token("justsha"), ("justsha", ""));
    }

    #[test]
    fn run_cmd_captures_stdout_and_passes_token() {
        let tmp = std::env::temp_dir().join(format!("cw-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // No token: plain capture.
        assert_eq!(run_cmd(&tmp, "echo hello", None).unwrap(), "hello");
        // The token arrives as $CRUCIBLE_TOKEN (the restore path).
        assert_eq!(
            run_cmd(&tmp, "printf '%s' \"$CRUCIBLE_TOKEN\"", Some("ENVTOK")).unwrap(),
            "ENVTOK"
        );
        // Nonzero exit is an error.
        assert!(run_cmd(&tmp, "exit 3", None).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn composite_snapshots_and_restores_each_component() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("composite-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("f.txt"), "a0").unwrap();
        fs::write(b.join("f.txt"), "b0").unwrap();
        crucible_vcs::vcs::ensure_repo(&a).unwrap();
        crucible_vcs::vcs::ensure_repo(&b).unwrap();

        // restore_cmd records the domain token it received, so we can assert it round-tripped.
        let domtok_file = root.join("domtok-seen");
        let world = CompositeWorld {
            components: vec![("a".into(), a.clone()), ("b".into(), b.clone())],
            base_dir: root.clone(),
            apply_cmd: None,
            snapshot_cmd: Some("echo DOMTOK".to_string()),
            restore_cmd: Some(format!(
                "printf '%s' \"$CRUCIBLE_TOKEN\" > {}",
                domtok_file.display()
            )),
        };

        // Baseline: a valid JSON token naming both components + the combined domain token.
        let base = world.snapshot("baseline").unwrap();
        let parsed: CompositeToken = serde_json::from_str(&base).unwrap();
        assert_eq!(parsed.components.len(), 2, "both component shas bundled");
        assert_eq!(
            parsed.domain, "DOMTOK",
            "combined snapshot_cmd token captured"
        );

        // Edit BOTH component trees and commit them as a new snapshot (the kept-tuple case).
        fs::write(a.join("f.txt"), "a1-edited").unwrap();
        fs::write(b.join("f.txt"), "b1-edited").unwrap();
        world.snapshot("turn1").unwrap();
        assert_eq!(fs::read_to_string(a.join("f.txt")).unwrap(), "a1-edited");

        // Restore to baseline: BOTH trees roll back, and the domain token reaches restore_cmd.
        world.restore(&base).unwrap();
        assert_eq!(fs::read_to_string(a.join("f.txt")).unwrap(), "a0");
        assert_eq!(fs::read_to_string(b.join("f.txt")).unwrap(), "b0");
        assert_eq!(fs::read_to_string(&domtok_file).unwrap(), "DOMTOK");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn composite_staged_diff_captures_every_component() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("composite-diff-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("f.txt"), "a0\n").unwrap();
        fs::write(b.join("f.txt"), "b0\n").unwrap();
        crucible_vcs::vcs::ensure_repo(&a).unwrap();
        crucible_vcs::vcs::ensure_repo(&b).unwrap();

        let world = CompositeWorld {
            components: vec![("vllm".into(), a.clone()), ("epp".into(), b.clone())],
            base_dir: root.clone(),
            apply_cmd: None,
            snapshot_cmd: None,
            restore_cmd: None,
        };

        // No edits yet -> empty diff.
        assert_eq!(world.staged_diff(), (String::new(), String::new()));

        // The agent edits BOTH component trees this turn (the cross-cut case).
        fs::write(a.join("f.txt"), "a0\nVLLM-EDIT\n").unwrap();
        fs::write(b.join("f.txt"), "b0\nEPP-EDIT\n").unwrap();

        let (diff, stat) = world.staged_diff();
        // The combined diff attributes each component and includes both edits, the empty-diff bug
        // (a single base-dir diff) would miss both, since the base dir isn't a git repo.
        assert!(
            diff.contains("=== component: vllm ==="),
            "vllm header: {diff}"
        );
        assert!(
            diff.contains("=== component: epp ==="),
            "epp header: {diff}"
        );
        assert!(diff.contains("VLLM-EDIT"), "vllm edit in diff: {diff}");
        assert!(diff.contains("EPP-EDIT"), "epp edit in diff: {diff}");
        assert!(
            stat.contains("vllm:") && stat.contains("epp:"),
            "per-component shortstat: {stat}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn publish_components_pins_per_component_base_and_head_and_drops_untouched() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("composite-publish-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let a = root.join("vllm");
        let b = root.join("epp");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("f.txt"), "a0\n").unwrap();
        fs::write(b.join("f.txt"), "b0\n").unwrap();
        crucible_vcs::vcs::ensure_repo(&a).unwrap();
        crucible_vcs::vcs::ensure_repo(&b).unwrap();

        let world = CompositeWorld {
            components: vec![("vllm".into(), a.clone()), ("epp".into(), b.clone())],
            base_dir: root.clone(),
            apply_cmd: None,
            snapshot_cmd: None,
            restore_cmd: None,
        };

        // Baseline pins both components' pristine shas.
        let base_snap = world.snapshot("baseline").unwrap();
        // The agent edits ONLY vllm this turn; commit the kept tuple.
        fs::write(a.join("f.txt"), "a0\nVLLM-EDIT\n").unwrap();
        let best_snap = world.snapshot("turn1").unwrap();
        let targets = world
            .publish_components(&base_snap, &best_snap)
            .expect("composite world yields publish targets");
        // epp is untouched (base == head) so it's dropped; only vllm gets a PR.
        assert_eq!(targets.len(), 1, "only the touched component publishes");
        let t = &targets[0];
        assert_eq!(t.name, "vllm");
        assert_eq!(t.workspace, a);
        assert_ne!(t.base_sha, t.head_sha, "head advanced past base");

        // The pinned shas match what each token actually recorded.
        let base: CompositeToken = serde_json::from_str(&base_snap).unwrap();
        let best: CompositeToken = serde_json::from_str(&best_snap).unwrap();
        let base_vllm = &base.components.iter().find(|(n, _)| n == "vllm").unwrap().1;
        let head_vllm = &best.components.iter().find(|(n, _)| n == "vllm").unwrap().1;
        assert_eq!(&t.base_sha, base_vllm, "base sha is the baseline token's");
        assert_eq!(&t.head_sha, head_vllm, "head sha is the best token's");

        let _ = fs::remove_dir_all(&root);
    }
}

//! The intent-to-pack drafting seam: the controller shells the engine's `crucible draft`
//! subcommand and reads the pack it leaves behind. The controller never runs an agent itself (the
//! loop image carries no agent CLI), so the drafting turn lives in the engine like `rank-grounded`
//! and this module is only the subprocess boundary. The intent travels in a file, never argv, and
//! the pack lands in a scratch directory this module reads — nothing the turn wrote reaches a
//! draft until `save_version` compiles and stores it. The turn's stderr is progress NDJSON while
//! it runs; this module streams it into the per-draft log the studio polls.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use crucible_contract::draft::{DraftProgress, DraftReport};
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::io::BufReader;

/// One completed drafting turn: what the engine reported and the pack it left behind.
pub(crate) struct GeneratedDraft {
    pub report: DraftReport,
    pub files: BTreeMap<String, String>,
}

/// How many progress lines one generate keeps before the oldest drop off.
const PROGRESS_LINES_KEPT: usize = 1024;

/// The progress of one draft's generate, appended to live as the engine's stderr reports it. Kept
/// after the turn ends so a poll racing the response still reads the tail.
#[derive(Default)]
pub(crate) struct DraftProgressLog {
    lines: Mutex<Vec<DraftProgress>>,
    intent: Mutex<Option<String>>,
    running: AtomicBool,
}

impl DraftProgressLog {
    fn push(&self, entry: DraftProgress) {
        let mut lines = self
            .lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lines.len() == PROGRESS_LINES_KEPT {
            lines.remove(0);
        }
        lines.push(entry);
    }

    /// The lines so far, oldest first.
    pub(crate) fn snapshot(&self) -> Vec<DraftProgress> {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the generate this log belongs to is still running.
    pub(crate) fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// The intent this log's generate was given, when it said one.
    pub(crate) fn intent(&self) -> Option<String> {
        self.intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The window a generate is running in, held by the request that started it. Dropping it — on
/// success, on a refusal, on a panic — is the only thing that ends the window, so no exit path
/// can leave the studio polling a turn that already finished.
pub(crate) struct RunningTurn {
    log: Arc<DraftProgressLog>,
}

impl RunningTurn {
    /// The log this turn streams into.
    pub(crate) fn log(&self) -> Arc<DraftProgressLog> {
        Arc::clone(&self.log)
    }
}

impl Drop for RunningTurn {
    fn drop(&mut self) {
        self.log.running.store(false, Ordering::Release);
    }
}

/// Every draft's latest generate log, keyed by draft id. `begin` starts a fresh log; an entry
/// stays until that draft's next generate replaces it.
#[derive(Default)]
pub(crate) struct DraftProgressRegistry {
    entries: Mutex<HashMap<String, Arc<DraftProgressLog>>>,
}

impl DraftProgressRegistry {
    /// Start a fresh progress log for `id`'s generate, replacing whatever an earlier generate
    /// left. The log the call returns is the one the generate streams into, and it keeps the
    /// intent it was begun with so a page reloaded mid-turn can restore what the operator typed.
    pub(crate) fn begin(&self, id: &str, intent: &str) -> RunningTurn {
        let log = Arc::new(DraftProgressLog {
            lines: Mutex::new(Vec::new()),
            intent: Mutex::new(Some(intent.to_string())),
            running: AtomicBool::new(true),
        });
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), Arc::clone(&log));
        RunningTurn { log }
    }

    /// The log `begin` last handed out for `id`, when this draft has generated at least once.
    pub(crate) fn get(&self, id: &str) -> Option<Arc<DraftProgressLog>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }
}

/// Run one `crucible draft` turn for `intent`, seeded with `context` (the draft's current files;
/// empty on a fresh draft). The spawn goes through the contract gate like every other engine
/// subprocess: a version-mismatched engine is refused here, before anything spends. `progress`,
/// when the studio is watching, receives the turn's stderr lines as they arrive.
pub(crate) async fn generate(
    contracts: &crate::runs::contract::ContractRegistry,
    intent: &str,
    context: &BTreeMap<String, String>,
    max_cost: f64,
    refine_rounds: u32,
    progress: Option<Arc<DraftProgressLog>>,
) -> Result<GeneratedDraft> {
    let bin = crate::runs::engine::resolve_bin();
    contracts
        .admit_all(
            crate::runs::contract::RequestKind::Draft,
            &[crate::runs::contract::DispatchTarget::Binary(bin.clone())],
        )
        .await?;
    let scratch = tempfile::tempdir().context("a scratch dir for the drafting turn")?;
    let goal = scratch.path().join("intent.md");
    std::fs::write(&goal, intent).with_context(|| format!("writing {}", goal.display()))?;
    let context_dir = scratch.path().join("context");
    if !context.is_empty() {
        write_context(&context_dir, context)?;
    }
    let out = scratch.path().join("pack");
    std::fs::create_dir(&out).with_context(|| format!("creating {}", out.display()))?;

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args([
        "draft",
        "--json",
        "--goal-file",
        &goal.to_string_lossy(),
        "--out",
        &out.to_string_lossy(),
        "--max-cost",
        &max_cost.to_string(),
        "--refine-rounds",
        &refine_rounds.to_string(),
    ]);
    if let Some(model) = draft_model() {
        cmd.arg("--model").arg(model);
    }
    if let Some(effort) = draft_effort() {
        cmd.arg("--effort").arg(effort);
    }
    if !context.is_empty() {
        cmd.arg("--context").arg(&context_dir);
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{} draft`", bin.display()))?;
    // Both pipes are drained concurrently: a turn whose stdout report backs up would otherwise
    // block mid-write while this side still reads stderr.
    let stderr_pipe = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut whole = String::new();
        let Some(pipe) = stderr_pipe else {
            return whole;
        };
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            whole.push_str(&line);
            whole.push('\n');
            let entry = crucible_contract::json::from_str::<DraftProgress>(line.trim()).ok();
            if let (Some(entry), Some(log)) = (entry, progress.as_ref()) {
                log.push(entry);
            }
        }
        whole
    });
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .await
            .with_context(|| "reading `crucible draft --json` stdout")?;
    }
    let status = child
        .wait()
        .await
        .with_context(|| "waiting on `crucible draft --json`")?;
    let stderr = stderr_task
        .await
        .context("the drafting turn's stderr reader died")?;
    let report = parse_report(&stdout).with_context(|| {
        format!(
            "parsing `crucible draft --json` output (exit {:?}): stderr={stderr:?}",
            status.code(),
        )
    })?;
    let tree = crucible_contract::pack_tree::walk_dir(&out)
        .context("reading the pack the drafting turn left")?
        .tree;
    let files = crate::playbooks::packs::text_files(tree).context("reading the drafted pack")?;
    Ok(GeneratedDraft { report, files })
}

/// `CRUCIBLE_DRAFT_MODEL`: the model this controller's drafting turns run on. Operator
/// configuration, not per-request input — the studio's Generate stays one button.
fn draft_model() -> Option<String> {
    std::env::var("CRUCIBLE_DRAFT_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// `CRUCIBLE_DRAFT_EFFORT`: the turns' reasoning-effort tier, spelled as the engine's
/// `--effort` value (low|medium|high|xhigh|max).
fn draft_effort() -> Option<String> {
    std::env::var("CRUCIBLE_DRAFT_EFFORT")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Parse the last JSON line `crucible draft --json` prints. The engine exits nonzero when the pack
/// never compiled or blew the budget but still prints the report, so the exit status is not the
/// contract — this document is.
fn parse_report(stdout: &str) -> Result<DraftReport> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .with_context(|| format!("crucible draft printed no report: {stdout:?}"))?;
    crucible_contract::json::from_str(line.trim())
        .with_context(|| format!("crucible draft output is not a report: {line:?}"))
}

/// Write the draft's current files under `dir`. The paths come from a stored pack tree, so they are
/// pack-relative by construction; no path here is caller-supplied.
fn write_context(dir: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (path, body) in files {
        let dest = dir.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&dest, body).with_context(|| format!("writing {path}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A progress log keeps only its newest lines; a generate that talks a lot cannot grow it
    /// without bound.
    #[test]
    fn a_progress_log_drops_its_oldest_lines_past_the_cap() {
        let log = DraftProgressLog::default();
        for round in 1..=(PROGRESS_LINES_KEPT + 5) {
            log.push(DraftProgress {
                round: 1,
                line: format!("line {round}"),
            });
        }
        let snapshot = log.snapshot();
        assert_eq!(snapshot.len(), PROGRESS_LINES_KEPT);
        assert_eq!(snapshot.first().map(|e| e.line.as_str()), Some("line 6"));
        let last = format!("line {}", PROGRESS_LINES_KEPT + 5);
        assert_eq!(
            snapshot.last().map(|e| e.line.as_str()),
            Some(last.as_str())
        );
    }

    /// A turn's running window closes when the request that started it goes away, whatever way
    /// it went; the registry keeps the lines and the intent for a poll racing the response.
    #[test]
    fn a_running_turn_closes_when_its_guard_drops() {
        let registry = DraftProgressRegistry::default();
        let turn = registry.begin("d", "sweep the batch size");
        let log = turn.log();
        assert!(log.is_running());
        assert_eq!(log.intent().as_deref(), Some("sweep the batch size"));
        log.push(DraftProgress {
            round: 1,
            line: "tool Write: crucible.toml".to_string(),
        });
        drop(turn);
        assert!(!log.is_running());
        let kept = registry.get("d").expect("kept");
        assert_eq!(kept.snapshot().len(), 1);
        assert!(!kept.is_running());
        assert_eq!(kept.intent().as_deref(), Some("sweep the batch size"));
    }

    /// The engine's own report shape, decoded the way the controller decodes it.
    #[test]
    fn the_report_line_decodes() {
        let stdout = "some agent chatter\n{\"ok\":true,\"cost_usd\":0.5,\"over_budget\":false,\"rounds\":2,\"findings\":[],\"declined\":null}\n";
        let report = parse_report(stdout).expect("report");
        assert!(report.ok);
        assert_eq!(report.cost_usd, 0.5);
        assert_eq!(report.rounds, 2);
    }

    #[test]
    fn a_report_that_never_printed_is_an_error() {
        assert!(parse_report("").is_err());
        assert!(parse_report("not json at all").is_err());
    }

    #[test]
    fn context_files_land_under_their_nested_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut files = BTreeMap::new();
        files.insert("crucible.toml".to_string(), "[agent]\n".to_string());
        files.insert(
            "prompts/draft.md".to_string(),
            "write the roundup".to_string(),
        );
        write_context(dir.path(), &files).expect("write");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("prompts/draft.md")).expect("nested"),
            "write the roundup"
        );
    }
}

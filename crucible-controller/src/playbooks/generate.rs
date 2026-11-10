//! The intent-to-pack drafting seam: the controller shells the engine's `crucible draft`
//! subcommand and reads the pack it leaves behind. The controller never runs an agent itself (the
//! loop image carries no agent CLI), so the drafting turn lives in the engine like `rank-grounded`
//! and this module is only the subprocess boundary. The intent travels in a file, never argv, and
//! the pack lands in a scratch directory this module reads — nothing the turn wrote reaches a
//! draft until `save_version` compiles and stores it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};
use crucible_contract::draft::DraftReport;

/// One completed drafting turn: what the engine reported and the pack it left behind.
pub(crate) struct GeneratedDraft {
    pub report: DraftReport,
    pub files: BTreeMap<String, String>,
}

/// Run one `crucible draft` turn for `intent`, seeded with `context` (the draft's current files;
/// empty on a fresh draft). The spawn goes through the contract gate like every other engine
/// subprocess: a version-mismatched engine is refused here, before anything spends.
pub(crate) async fn generate(
    contracts: &crate::runs::contract::ContractRegistry,
    intent: &str,
    context: &BTreeMap<String, String>,
    max_cost: f64,
    refine_rounds: u32,
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
    if !context.is_empty() {
        cmd.arg("--context").arg(&context_dir);
    }
    let output = cmd
        .output()
        .await
        .with_context(|| format!("spawning `{} draft`", bin.display()))?;
    let report = parse_report(&String::from_utf8_lossy(&output.stdout)).with_context(|| {
        format!(
            "parsing `crucible draft --json` output (exit {:?}): stderr={:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        )
    })?;
    let tree = crucible_contract::pack_tree::walk_dir(&out)
        .context("reading the pack the drafting turn left")?
        .tree;
    let files = crate::playbooks::packs::text_files(tree).context("reading the drafted pack")?;
    Ok(GeneratedDraft { report, files })
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

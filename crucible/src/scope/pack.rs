use crate::manifest;
use crate::manifest::AgentBackend;
use crate::scope::cli::ScopeReport;
use anyhow::{Context, Result};
use base64::Engine as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// A pack that cannot be written or shipped. The two cap arms are the reason the marker
/// protocol stays honest: a truncated blob would look like a real pack to the controller.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error(
        "--out {} already exists and is non-empty (pass --force to propose into it anyway)",
        .path.display()
    )]
    OutDirNotEmpty { path: std::path::PathBuf },
    #[error(
        "pack tar is {bytes} bytes, over the {PACK_TAR_CAP_BYTES}-byte cap — refusing to emit a \
         pack that can't ride the pod logs whole"
    )]
    TarOverCap { bytes: usize },
    #[error(
        "pack payload is {encoded} bytes encoded ({tarred} bytes tarred), over the \
         {PACK_PAYLOAD_CAP_BYTES}-byte cap — refusing to emit a pack that can't ride the pod \
         logs whole"
    )]
    PayloadOverCap { encoded: usize, tarred: usize },
}

/// The prefix of the single-line pack marker a SURVIVING `--marker` scope turn emits before the
/// report marker: base64 of the gzipped tar of the frozen pack directory. With `scope_executor =
/// pod` this is the pack's only way home, the pod's filesystem dies with it, and the controller
/// must land the pack at its own `pack_out` to push the approval-PR branches and render the run
/// pod. The payload is an `{"error":…}` JSON object instead when the pack couldn't be emitted
/// whole (oversize), so the controller fails the scope loudly rather than using a truncated pack.
/// Shares the controller's scraper literal via `crucible-contract`.
pub use crucible_contract::SCOPE_PACK_MARKER;

/// Sanity cap on the pack tar (uncompressed bytes), anything bigger is a drafted pack gone wrong
/// (a vendored dependency tree, a stray dataset). The budget that actually matters is
/// [`PACK_PAYLOAD_CAP_BYTES`] on the emitted marker payload: a text-heavy pack many times the old
/// 4 MiB tar bound still compresses under the log budget, so the raw-tar bound is deliberately
/// loose. Over either cap the emit refuses with an explicit error, never a silently truncated pack.
pub(super) const PACK_TAR_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Cap on the emitted pack marker payload (base64(gzip(tar)) bytes), the thing that must ride one
/// pod-log line under the kubelet's 10 MiB log-rotation budget with headroom for the rest of the
/// turn's output. The raw tar size is the wrong stage to bound: a 12 MiB text pack gzips to a
/// fraction of this, and the payload is what the scraper must recover whole.
pub(super) const PACK_PAYLOAD_CAP_BYTES: usize = 6 * 1024 * 1024;

/// The pack's private calibration dir. It holds the `[judge.selftest]` reference fix (the working
/// answer `good_cmd` applies to prove the gate discriminates). It exists only during scoping; the
/// freeze strips it before the pack ships, because a run agent that reads it is handed the solution,
/// the exact context poisoning the loop must not measure through.
pub(super) const CONTROLS_DIR: &str = "_controls";

/// Where the propose/refine turns write the pack, *inside* the turn's workspace (the `--repo`
/// scratch checkout, the agent's cwd). The invariant this encodes: any path the agent must write
/// and the host must later read has to live under `paths.workspace`, because on the openshell
/// backend only the workspace round-trips the sandbox (upload before the turn, download after),
/// a pack written to the host-absolute `--out` path lands in the sandbox's own filesystem and
/// evaporates with it. The host mirrors this dir onto `--out` after every turn
/// ([`sync_pack_from_workspace`]).
pub(super) const PACK_WORK_DIR: &str = "_scope_out";

/// Mirror the turn's workspace pack dir (`<scratch>/{PACK_WORK_DIR}`) onto the host `--out` dir
/// after every propose/refine turn. This is the host side of the sandbox round-trip: the agent
/// writes only under its workspace (the one dir the openshell backend uploads and downloads),
/// and the pipeline reads only `--out`, so the mirror is what connects them, on the local
/// backend it's a plain same-filesystem copy. A mirror (clear then copy), not a merge: a refine
/// round that deletes a file must not leave the stale copy behind for `crucible check` to read.
pub(super) fn sync_pack_from_workspace(scratch: &Path, pack: &Path) -> Result<()> {
    let work = scratch.join(PACK_WORK_DIR);
    if !work.is_dir() {
        // The turn wrote nothing (and removed the pre-created dir); the structure check reports
        // the missing manifest as this round's evidence.
        return Ok(());
    }
    clear_dir(pack).with_context(|| format!("clearing --out {}", pack.display()))?;
    copy_tree(&work, pack)
        .with_context(|| format!("mirroring {} -> {}", work.display(), pack.display()))
}

/// Remove everything inside `dir` without removing `dir` itself (the pipeline holds its
/// canonicalized path).
fn clear_dir(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        let ty = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if ty.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing {}", path.display()))?;
        } else {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Recursively copy `src`'s contents into `dst` (created if missing). `fs::copy` preserves
/// permissions, which the pack's executable measure scripts depend on.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry.with_context(|| format!("reading {}", src.display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ty = entry
            .file_type()
            .with_context(|| format!("stat {}", from.display()))?;
        if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Read the origin clone URL and HEAD SHA of the `--repo` checkout the scope turn ran against. In
/// a turn pod `--repo` is `/tmp/crucible-turn-checkout`, a fresh clone of the upstream repo: its
/// `origin` is the upstream clone URL and its HEAD is the SHA the turn was pinned to. Pack freeze
/// pins BOTH into `[repo]` (`url` + `ref`), so the run pod clones the same code from a real URL
/// instead of the scope pod's local checkout path (which doesn't exist over there).
///
/// Off the pod it degrades gracefully: no `origin` (a bare local fixture) pins the checkout's
/// absolute path as the url; a non-checkout `--repo` (a URL passed straight through in dev) pins it
/// verbatim with no ref.
pub(super) fn repo_pin(repo: &str) -> (String, Option<String>) {
    let url = git_capture(repo, &["remote", "get-url", "origin"])
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let p = Path::new(repo);
            if p.is_dir() {
                std::fs::canonicalize(p)
                    .map(|a| a.display().to_string())
                    .unwrap_or_else(|_| repo.to_string())
            } else {
                repo.to_string()
            }
        });
    let sha = git_capture(repo, &["rev-parse", "HEAD"]).filter(|s| !s.is_empty());
    (url, sha)
}

/// `git -C <dir> <args>` capturing trimmed stdout, `None` on any failure (missing repo, no origin,
/// no commit), every caller has a sane fallback for the absent case.
fn git_capture(dir: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Rewrite a freshly-drafted pack manifest into a RUNNABLE one, in place, preserving the agent's
/// comments and key order (a targeted `toml_edit` surgery, not a serde round-trip, the `Manifest`
/// struct is deserialize-only and would drop every comment). Three fixes, each one a live failure
/// the first approval checkpoint build hit before the run phase could even start:
///
///  1. `[repo]`, pin `url` (the checkout's `origin`) + `ref` (its HEAD SHA), dropping whatever
///     local `path` the propose turn wrote (that path doesn't exist in the run pod, which clones
///     `[repo]` itself in `run::manifest_setup`).
///  2. `[deploy]`, inject an empty table if absent; `deploy::render::from_manifest` requires the
///     block even when every `DeployCfg` field is defaulted (a local-benchmark domain).
///  3. `[agent]`, when the scope turn itself ran sandboxed (`--agent-backend openshell
///     --sandbox-image X`), pin the run pod to the same backend + image; the drafted `backend =
///     "local"` was never exercised (the scope argv overrode it) and the run pod's openshell-loop
///     template requires the sandboxed backend + image.
pub(super) fn normalize_frozen_manifest(
    manifest_path: &Path,
    repo: &str,
    agent_backend: AgentBackend,
    sandbox_image: Option<&str>,
) -> Result<()> {
    use toml_edit::{DocumentMut, Item, Table, value};
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {} to normalize it", manifest_path.display()))?;
    let mut doc: DocumentMut = text.parse().with_context(|| {
        format!(
            "parsing {} as TOML to normalize it",
            manifest_path.display()
        )
    })?;

    // 1. Pin [repo] to url + ref.
    let (url, sha) = repo_pin(repo);
    let repo_tbl = doc
        .entry("repo")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("[repo] is present but not a table")?;
    repo_tbl.remove("path");
    repo_tbl.insert("url", value(url));
    match sha {
        Some(sha) => {
            repo_tbl.insert("ref", value(sha));
        }
        None => {
            repo_tbl.remove("ref");
        }
    }

    // 2. Ensure [deploy] exists (an empty block is valid for a no-rebuild domain).
    if doc.get("deploy").is_none() {
        doc.insert("deploy", Item::Table(Table::new()));
    }

    // 3. Pin the sandboxed backend when the scope turn itself ran sandboxed.
    if agent_backend == AgentBackend::Openshell
        && let Some(img) = sandbox_image
    {
        let agent_tbl = doc
            .entry("agent")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("[agent] is present but not a table")?;
        agent_tbl.insert("backend", value("openshell"));
        agent_tbl.insert("sandbox_image", value(img));
    }

    std::fs::write(manifest_path, doc.to_string())
        .with_context(|| format!("writing the normalized {}", manifest_path.display()))
}

/// The frozen pack's `[workspace].dir` (default `workspace`), read from whichever manifest shape it
/// is, the one directory freeze must drop from the pack even though `crucible check` populated it.
pub(super) fn frozen_workspace_dir(manifest_path: &Path) -> String {
    let fallback = || "workspace".to_string();
    if manifest::is_composite(manifest_path) {
        manifest::CompositeManifest::load(manifest_path)
            .map(|m| m.workspace.dir)
            .unwrap_or_else(|_| fallback())
    } else {
        manifest::Manifest::load(manifest_path)
            .map(|m| m.workspace.dir)
            .unwrap_or_else(|_| fallback())
    }
}

/// Strip the run pod's own checkout out of the frozen pack: the manifest's `[workspace].dir` (the
/// tree `crucible check` set up during validation) plus any other top-level directory that is
/// itself a git checkout. The run leg rebuilds the workspace by cloning the pinned `[repo]`, so a
/// checkout shipped in the pack is dead weight, and #137 only kept `.git` out of the emitted TAR,
/// leaving the whole tree in `--out` and the pack PR.
pub(super) fn drop_repo_checkouts(pack: &Path, workspace_dir: &str) -> Result<()> {
    let configured = pack.join(workspace_dir);
    if configured.is_dir() {
        std::fs::remove_dir_all(&configured)
            .with_context(|| format!("dropping the pack workspace {}", configured.display()))?;
    }
    for entry in std::fs::read_dir(pack).with_context(|| format!("reading {}", pack.display()))? {
        let entry = entry.with_context(|| format!("reading {}", pack.display()))?;
        let path = entry.path();
        if path.is_dir() && path.join(".git").exists() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("dropping the checkout {}", path.display()))?;
        }
    }
    Ok(())
}

/// De-prescribe the frozen pack: strip its private calibration (`_controls/`) and the
/// `[judge.selftest]` block that references it, so neither ships to the run phase.
///
/// WHY: the self-test proves the gate discriminates by applying a *reference fix*, a complete,
/// working answer (`good_cmd`'s `_controls/…` patch, injected into the run workspace). Shipping
/// that is shipping an answer key: a run agent that reads it is no longer being measured on
/// autonomous solve capability, it's transcribing. So once the self-test has done its job (it ran
/// during the re-validation just before this), the controls come out, and the `[judge.selftest]`
/// block with them, because its cmds now point at an absent dir and a run-side `crucible check`
/// must not trip on a half-stripped selftest (an absent block is a plain warning, a broken one is a
/// finding). Any `[[workspace.inject]]` pulling `_controls/` into the workspace goes too, or the
/// run's setup would fail on the missing src.
pub(super) fn strip_controls_and_selftest(manifest_path: &Path, pack: &Path) -> Result<()> {
    use toml_edit::{DocumentMut, Item};
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {} to de-prescribe it", manifest_path.display()))?;
    let mut doc: DocumentMut = text.parse().with_context(|| {
        format!(
            "parsing {} as TOML to de-prescribe it",
            manifest_path.display()
        )
    })?;

    // Drop [judge.selftest]: its good_cmd/bad_cmd reference _controls/, which is about to be gone.
    if let Some(judge) = doc.get_mut("judge").and_then(Item::as_table_mut) {
        judge.remove("selftest");
    }

    // Drop any [[workspace.inject]] whose src pulls a control into the workspace.
    if let Some(inject) = doc
        .get_mut("workspace")
        .and_then(Item::as_table_mut)
        .and_then(|w| w.get_mut("inject"))
        .and_then(Item::as_array_of_tables_mut)
    {
        inject.retain(|t| !inject_src_is_control(t));
        if inject.is_empty()
            && let Some(ws) = doc.get_mut("workspace").and_then(Item::as_table_mut)
        {
            ws.remove("inject");
        }
    }

    std::fs::write(manifest_path, doc.to_string())
        .with_context(|| format!("writing the de-prescribed {}", manifest_path.display()))?;

    let controls = pack.join(CONTROLS_DIR);
    if controls.is_dir() {
        std::fs::remove_dir_all(&controls)
            .with_context(|| format!("dropping the pack controls dir {}", controls.display()))?;
    }
    Ok(())
}

/// True when an inject's `src` points at the private `_controls/` dir (with or without a `./`
/// prefix), the entries that would otherwise copy the reference fix into the run workspace.
fn inject_src_is_control(inject: &toml_edit::Table) -> bool {
    inject
        .get("src")
        .and_then(|v| v.as_str())
        .map(|s| {
            let s = s.trim_start_matches("./");
            s == CONTROLS_DIR || s.starts_with(&format!("{CONTROLS_DIR}/"))
        })
        .unwrap_or(false)
}

/// Refuse a non-empty `--out` unless `force`; create it if it doesn't exist yet.
pub(super) fn ensure_out_dir(out: &Path, force: bool) -> Result<()> {
    if !out.exists() {
        std::fs::create_dir_all(out)
            .with_context(|| format!("creating --out {}", out.display()))?;
        return Ok(());
    }
    let nonempty = std::fs::read_dir(out)
        .with_context(|| format!("reading --out {}", out.display()))?
        .next()
        .is_some();
    if nonempty && !force {
        return Err(PackError::OutDirNotEmpty {
            path: out.to_path_buf(),
        }
        .into());
    }
    Ok(())
}

pub(super) fn scratch_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "crucible-{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Tar the pack directory (entries relative to the dir), refusing over [`PACK_TAR_CAP_BYTES`],
/// the marker line must fit the pod-log budget, and a partial pack must never leave this process.
///
/// `.git` subtrees are EXCLUDED: the run phase builds its workspace by cloning `[repo]` from the
/// manifest (`run::manifest_setup`), so a checkout's object store inside the pack is dead weight,
/// and its packfiles are already compressed, which is exactly what blew the encoded-payload budget
/// on the first live checkpoint build (12.6 MiB pack, 7.4 MiB encoded, mostly git objects).
pub(super) fn tar_pack_dir(dir: &Path) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    append_dir_filtered(&mut builder, dir, Path::new(""))
        .with_context(|| format!("tarring the pack dir {}", dir.display()))?;
    let tar = builder
        .into_inner()
        .context("finishing the pack tar stream")?;
    if tar.len() > PACK_TAR_CAP_BYTES {
        return Err(PackError::TarOverCap { bytes: tar.len() }.into());
    }
    Ok(tar)
}

/// Recursively append `dir`'s entries under `prefix` (both relative to the pack root), skipping
/// any directory named `.git`. Empty directories are dropped (a pack never depends on one) and a
/// symlink is appended as the file it points at, pack content is plain files by construction.
fn append_dir_filtered(
    builder: &mut tar::Builder<Vec<u8>>,
    dir: &Path,
    prefix: &Path,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        let name = entry.file_name();
        let rel = prefix.join(&name);
        if path.is_dir() {
            if name == ".git" {
                continue;
            }
            append_dir_filtered(builder, &path, &rel)?;
        } else {
            builder
                .append_path_with_name(&path, &rel)
                .with_context(|| format!("tarring {}", path.display()))?;
        }
    }
    Ok(())
}

/// The pack marker's payload for a surviving pack: base64(gzip(tar of the pack dir)), refused over
/// [`PACK_PAYLOAD_CAP_BYTES`], the encoded line is what must survive the pod-log budget whole.
pub(super) fn pack_marker_payload(dir: &Path) -> Result<String> {
    let tar = tar_pack_dir(dir)?;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar).context("gzipping the pack tar")?;
    let gz = enc.finish().context("finishing the pack gzip stream")?;
    let payload = base64::engine::general_purpose::STANDARD.encode(gz);
    if payload.len() > PACK_PAYLOAD_CAP_BYTES {
        return Err(PackError::PayloadOverCap {
            encoded: payload.len(),
            tarred: tar.len(),
        }
        .into());
    }
    Ok(payload)
}

/// The raw gzipped tar of the pack dir, the Tier 2 `scope-pack` artifact body (uploaded to the
/// ingest drop-box). No base64, no line-length cap: the drop-box takes the compressed bytes directly
/// and enforces its own 16 MiB HTTP cap, so this doesn't fight the pod-log budget the way
/// [`pack_marker_payload`] must.
pub(super) fn pack_gz(dir: &Path) -> Result<Vec<u8>> {
    let tar = tar_pack_dir(dir)?;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar).context("gzipping the pack tar")?;
    enc.finish().context("finishing the pack gzip stream")
}

/// The full `CRUCIBLE_SCOPE_PACK: …` marker line for a report, or `None` when the pipeline didn't
/// survive (a dead proposal has no pack to deliver). Survival is the same predicate the
/// controller's `ScopeReport::survived` applies: a frozen digest and no failed stage. An emit
/// failure (oversize pack) becomes an `{"error":…}` payload on the same line, the controller must
/// fail the scope loudly, never silently use a truncated pack.
pub(super) fn pack_marker_line(report: &ScopeReport, pack: &Path) -> Option<String> {
    let survived = report.digest.is_some() && report.stages.iter().all(|r| r.passed);
    if !survived {
        return None;
    }
    let payload = match pack_marker_payload(pack) {
        Ok(b64) => b64,
        Err(e) => serde_json::json!({ "error": format!("{e:#}") }).to_string(),
    };
    Some(format!("{SCOPE_PACK_MARKER} {payload}"))
}

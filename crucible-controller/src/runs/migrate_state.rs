//! `crucible-controller db migrate-state`: the one-shot cutover that walks a legacy state volume
//! and loads it into the shared-state tables — drop-box evidence into artifact chunks (digests
//! re-verified against the `pod_artifacts` pointers), run session logs (dispatched, adopted,
//! external) into run-session artifacts with `runs.session_uri` repointed at the `db://` scheme,
//! pack working trees re-tarred deterministically into `pack_tarballs` (trailing `STEER.md`
//! steering appends split into `pack_steering` so the frozen pack stays frozen),
//! `controller-events.jsonl` into the `events` table, and `autopilot.json` into its row.
//!
//! Idempotent: every category short-circuits on a digest/content match, and event lines dedupe on
//! the full line tuple `(ts, key, from, to, reason, evidence, actor)` — old lines carry nothing
//! more unique than that, so two byte-identical lines collapse while two same-second transitions
//! that differ anywhere stay distinct. The run refuses to start unless it can take the
//! [`crate::client::MAINTENANCE_ADVISORY_LOCK`], which a running daemon holds for its lifetime
//! and the other maintenance commands hold for their run — so it can never race either.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail, ensure};
use crucible_contract::{ArtifactKind, content_digest};
use serde::Deserialize;
use sqlx::PgPool;
use std::path::Path;

/// One category's tally. `failed` carries a human-readable line per item (the path or key plus
/// the error), so the CLI can list every failure before exiting nonzero.
#[derive(Debug, Default)]
pub struct CategoryReport {
    pub migrated: usize,
    pub skipped: usize,
    pub failed: Vec<String>,
    /// Events only: unparseable `controller-events.jsonl` lines. A failure unless
    /// `skip_bad_lines`, in which case they are counted here and reported.
    pub bad_lines: usize,
}

/// The whole run's per-category tallies.
#[derive(Debug, Default)]
pub struct MigrateReport {
    pub evidence: CategoryReport,
    pub sessions: CategoryReport,
    pub packs: CategoryReport,
    pub steering: CategoryReport,
    pub events: CategoryReport,
    pub autopilot: CategoryReport,
}

impl MigrateReport {
    pub fn categories(&self) -> [(&'static str, &CategoryReport); 6] {
        [
            ("evidence", &self.evidence),
            ("run-sessions", &self.sessions),
            ("packs", &self.packs),
            ("steering", &self.steering),
            ("events", &self.events),
            ("autopilot", &self.autopilot),
        ]
    }

    pub fn failures(&self) -> Vec<&str> {
        self.categories()
            .into_iter()
            .flat_map(|(_, c)| c.failed.iter().map(String::as_str))
            .collect()
    }
}

enum Outcome {
    Migrated,
    Skipped,
}

/// Walk `state_dir` and load it into the shared-state tables. Item-level problems land in the
/// report's `failed` lists (the caller exits nonzero on any); an `Err` here is a structural
/// failure — the lock is held, the state dir is missing, or a whole category could not be read.
pub async fn migrate_state(
    pool: &PgPool,
    state_dir: &Path,
    skip_bad_lines: bool,
) -> Result<MigrateReport> {
    ensure!(
        state_dir.is_dir(),
        "the legacy state dir {} does not exist",
        state_dir.display()
    );
    let Some(lock) = crate::client::try_maintenance_lock(pool).await? else {
        bail!(
            "the maintenance advisory lock is held by another session (a running daemon or \
             another maintenance command); stop it and re-run"
        );
    };
    let result = migrate_locked(pool, state_dir, skip_bad_lines).await;
    if let Err(e) = lock.release().await {
        tracing::warn!(error = format!("{e:#}"), "maintenance-lock release failed");
    }
    result
}

async fn migrate_locked(
    pool: &PgPool,
    state_dir: &Path,
    skip_bad_lines: bool,
) -> Result<MigrateReport> {
    let mut report = MigrateReport::default();
    migrate_evidence(pool, state_dir, &mut report.evidence).await?;
    migrate_sessions(pool, state_dir, &mut report.sessions).await?;
    migrate_packs(pool, state_dir, &mut report.packs, &mut report.steering).await?;
    migrate_events(pool, state_dir, skip_bad_lines, &mut report.events).await?;
    migrate_autopilot(pool, state_dir, &mut report.autopilot).await?;
    Ok(report)
}

/// Directory entries of `dir`, sorted by name, or empty when `dir` is absent.
fn sorted_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("reading {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

// --- evidence/<pod>/<kind> -------------------------------------------------------------------

async fn migrate_evidence(pool: &PgPool, state_dir: &Path, rep: &mut CategoryReport) -> Result<()> {
    for pod_entry in sorted_entries(&state_dir.join("evidence"))? {
        let pod = pod_entry.file_name().to_string_lossy().into_owned();
        if pod == "external" || pod.starts_with('.') || !pod_entry.path().is_dir() {
            continue;
        }
        for file in sorted_entries(&pod_entry.path())? {
            let name = file.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !file.path().is_file() {
                continue;
            }
            match migrate_evidence_file(pool, &pod, &name, &file.path()).await {
                Ok(Outcome::Migrated) => rep.migrated += 1,
                Ok(Outcome::Skipped) => rep.skipped += 1,
                Err(e) => rep.failed.push(format!("{}: {e:#}", file.path().display())),
            }
        }
    }
    Ok(())
}

async fn migrate_evidence_file(
    pool: &PgPool,
    pod: &str,
    name: &str,
    path: &Path,
) -> Result<Outcome> {
    let kind: ArtifactKind = name
        .parse()
        .map_err(|e| anyhow::anyhow!("not an artifact kind: {e}"))?;
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let digest = content_digest(&bytes);
    let pointer = crate::runs::work_pods::pod_artifact(pool, pod, kind.as_str()).await?;
    if let Some(ptr) = &pointer {
        ensure!(
            ptr.digest == digest,
            "digest mismatch: pod_artifacts records {}, the file hashes to {digest}",
            ptr.digest
        );
    }
    let owner = crate::runs::blob_store::ArtifactOwner::PodEvidence {
        pod: pod.to_string(),
    };
    if crate::runs::blob_store::artifact_digest(pool, &owner, kind.as_str())
        .await?
        .as_deref()
        == Some(&digest)
    {
        return Ok(Outcome::Skipped);
    }
    let len = bytes.len() as i64;
    crate::runs::blob_store::put_artifact_bytes(
        pool,
        &owner,
        kind.as_str(),
        kind.max_bytes(),
        bytes,
    )
    .await
    .map_err(anyhow::Error::from)?;
    if pointer.is_none() {
        crate::runs::work_pods::record_pod_artifact(
            pool,
            &crate::runs::model::NewPodArtifact {
                pod: pod.to_string(),
                kind: kind.as_str().to_string(),
                digest,
                bytes: len,
            },
        )
        .await?;
    }
    Ok(Outcome::Migrated)
}

// --- runs/<id>/session.jsonl, adopted/, evidence/external/ ------------------------------------

async fn migrate_sessions(pool: &PgPool, state_dir: &Path, rep: &mut CategoryReport) -> Result<()> {
    let roots = [
        state_dir.join("runs"),
        state_dir.join("adopted"),
        state_dir.join("evidence").join("external"),
    ];
    for root in roots {
        for entry in sorted_entries(&root)? {
            let run_id = entry.file_name().to_string_lossy().into_owned();
            if run_id.starts_with('.') {
                continue;
            }
            let path = entry.path().join("session.jsonl");
            if !path.is_file() {
                continue;
            }
            match migrate_session(pool, &run_id, &path).await {
                Ok(Outcome::Migrated) => rep.migrated += 1,
                Ok(Outcome::Skipped) => rep.skipped += 1,
                Err(e) => rep.failed.push(format!("{}: {e:#}", path.display())),
            }
        }
    }
    Ok(())
}

async fn migrate_session(pool: &PgPool, run_id: &str, path: &Path) -> Result<Outcome> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = crate::runs::blob_store::gunzip_maybe(&raw)?;
    let outcome = match crate::runs::blob_store::get_run_session(pool, run_id).await? {
        Some(existing) if existing == text => Outcome::Skipped,
        _ => {
            crate::runs::blob_store::put_run_session(pool, run_id, text.as_bytes()).await?;
            Outcome::Migrated
        }
    };
    sqlx::query(
        r#"UPDATE runs SET session_uri = $1
           WHERE run_id = $2 AND (session_uri IS NULL
               OR (session_uri NOT LIKE 's3://%' AND session_uri NOT LIKE 'db://%'))"#,
    )
    .bind(crate::runs::blob_store::run_session_uri(run_id))
    .bind(run_id)
    .execute(pool)
    .await
    .context("repointing runs.session_uri")?;
    Ok(outcome)
}

// --- packs/<slug>/ ----------------------------------------------------------------------------

async fn migrate_packs(
    pool: &PgPool,
    state_dir: &Path,
    packs: &mut CategoryReport,
    steering: &mut CategoryReport,
) -> Result<()> {
    for entry in sorted_entries(&state_dir.join("packs"))? {
        let slug = entry.file_name().to_string_lossy().into_owned();
        if slug.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        if let Err(e) = migrate_pack(pool, &slug, &entry.path(), packs, steering).await {
            packs.failed.push(format!("{slug}: {e:#}"));
        }
    }
    Ok(())
}

async fn migrate_pack(
    pool: &PgPool,
    slug: &str,
    tree: &Path,
    packs: &mut CategoryReport,
    steering: &mut CategoryReport,
) -> Result<()> {
    let steer_path = tree.join("STEER.md");
    let (steer_override, blocks) = if steer_path.is_file() {
        let text = std::fs::read_to_string(&steer_path)
            .with_context(|| format!("reading {}", steer_path.display()))?;
        match split_steer(&text) {
            Ok((frozen, blocks)) => (Some(frozen), blocks),
            Err(e) => {
                tracing::warn!(
                    slug,
                    error = format!("{e:#}"),
                    "STEER.md steering split failed; taring the pack as-is"
                );
                (Some(text), Vec::new())
            }
        }
    } else {
        (None, Vec::new())
    };

    let created_ats: Vec<String> = blocks
        .iter()
        .map(|block| {
            Ok(crate::clock::stamp(
                jiff::Timestamp::from_second(block.epoch)
                    .with_context(|| format!("steer marker epoch {} out of range", block.epoch))?,
            ))
        })
        .collect::<Result<_>>()?;
    let tar_gz = deterministic_pack_tar(tree, steer_override.as_deref())?;
    let digest = content_digest(&tar_gz);
    let existing: Option<String> =
        sqlx::query_scalar("SELECT digest FROM pack_tarballs WHERE issue_slug = $1")
            .bind(slug)
            .fetch_optional(pool)
            .await
            .context("looking up the stored pack tarball")?;
    let store_tarball = match existing {
        Some(d) if d == digest => {
            packs.skipped += 1;
            false
        }
        Some(d) => bail!("stored tarball digest {d} differs from the re-tarred tree ({digest})"),
        None => true,
    };
    let import_blocks = if blocks.is_empty() {
        false
    } else {
        let existing_rows = crate::runs::blob_store::list_steering(pool, slug).await?;
        if existing_rows.is_empty() {
            true
        } else {
            tracing::warn!(
                slug,
                rows = existing_rows.len(),
                "pack_steering already has rows for this slug; not importing the STEER.md blocks"
            );
            steering.skipped += blocks.len();
            false
        }
    };
    if !store_tarball && !import_blocks {
        return Ok(());
    }

    let mut tx = pool.begin().await.context("begin pack import")?;
    if store_tarball {
        crate::runs::blob_store::put_pack_tarball(&mut *tx, slug, &tar_gz).await?;
    }
    if import_blocks {
        for (i, (block, created_at)) in blocks.iter().zip(&created_ats).enumerate() {
            sqlx::query(
                r#"INSERT INTO pack_steering (issue_slug, seq, body_md, author, created_at)
                   VALUES ($1, $2, $3, NULL, $4)"#,
            )
            .bind(slug)
            .bind((i + 1) as i64)
            .bind(&block.body)
            .bind(created_at)
            .execute(&mut *tx)
            .await
            .context("inserting a steering row")?;
        }
    }
    tx.commit().await.context("commit pack import")?;
    if store_tarball {
        packs.migrated += 1;
    }
    if import_blocks {
        steering.migrated += blocks.len();
    }
    Ok(())
}

/// One `<!-- steer @<secs> by control -->` block appended to a pack's `STEER.md`.
#[derive(Debug, PartialEq, Eq)]
struct SteerBlock {
    epoch: i64,
    body: String,
}

/// Split a `STEER.md` into the frozen prefix (everything before the first steer marker, byte-exact)
/// and the appended blocks. Errs when a marker line's epoch does not parse — the caller then keeps
/// the file whole.
fn split_steer(text: &str) -> Result<(String, Vec<SteerBlock>)> {
    let mut frozen = String::new();
    let mut blocks: Vec<SteerBlock> = Vec::new();
    for line in text.split_inclusive('\n') {
        match steer_marker_epoch(line)? {
            Some(epoch) => blocks.push(SteerBlock {
                epoch,
                body: String::new(),
            }),
            None => match blocks.last_mut() {
                Some(block) => block.body.push_str(line),
                None => frozen.push_str(line),
            },
        }
    }
    for block in &mut blocks {
        block.body = block.body.trim().to_string();
    }
    Ok((frozen, blocks))
}

/// The epoch of a steer marker line, `None` for any other line, `Err` for a marker whose epoch
/// does not parse.
fn steer_marker_epoch(line: &str) -> Result<Option<i64>> {
    let Some(rest) = line.trim_end().strip_prefix("<!-- steer @") else {
        return Ok(None);
    };
    let Some(middle) = rest.strip_suffix(" by control -->") else {
        return Ok(None);
    };
    middle
        .parse::<i64>()
        .map(Some)
        .with_context(|| format!("steer marker epoch {middle:?} does not parse"))
}

/// Gzip-tar a pack tree deterministically: entries sorted by name at every level, mtimes and
/// uid/gid zeroed, `.git` excluded, symlinks followed. `steer_override` replaces the root
/// `STEER.md`'s bytes (the frozen prefix, once the steering blocks are split into rows).
fn deterministic_pack_tar(tree: &Path, steer_override: Option<&str>) -> Result<Vec<u8>> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    add_dir(&mut builder, tree, Path::new(""), steer_override)?;
    let enc = builder.into_inner().context("finishing the pack tar")?;
    enc.finish().context("gzipping the pack tar")
}

fn add_dir(
    builder: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    dir: &Path,
    rel: &Path,
    steer_override: Option<&str>,
) -> Result<()> {
    for entry in sorted_entries(dir)? {
        let name = entry.file_name();
        let at_root = rel.as_os_str().is_empty();
        if at_root && name == ".git" {
            continue;
        }
        let path = entry.path();
        let entry_rel = rel.join(&name);
        if path.is_dir() {
            add_dir(builder, &path, &entry_rel, steer_override)?;
            continue;
        }
        let data = match steer_override {
            Some(s) if at_root && name == "STEER.md" => s.as_bytes().to_vec(),
            _ => std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        };
        let mode = std::fs::metadata(&path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        #[cfg(unix)]
        let mode = std::os::unix::fs::PermissionsExt::mode(&mode) & 0o7777;
        #[cfg(not(unix))]
        let mode = if mode.readonly() { 0o444 } else { 0o644 };
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        builder
            .append_data(&mut header, &entry_rel, data.as_slice())
            .with_context(|| format!("taring pack entry {}", entry_rel.display()))?;
    }
    Ok(())
}

// --- controller-events.jsonl ------------------------------------------------------------------

/// The frozen NDJSON line shape, tolerant of pre-`actor` lines and of non-status `from`/`to`
/// values (audit rows, drift markers) — every parseable line imports verbatim.
#[derive(Debug, Deserialize)]
struct LegacyEvent {
    v: i64,
    ts: String,
    key: String,
    from: String,
    to: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    evidence: Option<String>,
    #[serde(default)]
    actor: Option<String>,
}

async fn migrate_events(
    pool: &PgPool,
    state_dir: &Path,
    skip_bad_lines: bool,
    rep: &mut CategoryReport,
) -> Result<()> {
    let path = state_dir.join("controller-events.jsonl");
    if !path.is_file() {
        return Ok(());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut good: Vec<LegacyEvent> = Vec::new();
    let mut first_bad: Option<(usize, String)> = None;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LegacyEvent>(line) {
            Ok(ev) => good.push(ev),
            Err(e) => {
                rep.bad_lines += 1;
                first_bad.get_or_insert((i + 1, e.to_string()));
            }
        }
    }
    if rep.bad_lines > 0 && !skip_bad_lines {
        let (line_no, err) = first_bad.unwrap_or_default();
        rep.failed.push(format!(
            "{}: {} unparseable line(s), first at line {line_no}: {err} \
             (re-run with --skip-bad-lines to import the rest)",
            path.display(),
            rep.bad_lines,
        ));
        return Ok(());
    }
    let mut tx = pool.begin().await.context("begin events import")?;
    for ev in &good {
        let inserted = sqlx::query(
            r#"INSERT INTO events (v, ts, key, from_status, to_status, reason, evidence, actor)
               SELECT $1, $2, $3, $4, $5, $6, $7, $8
               WHERE NOT EXISTS (
                   SELECT 1 FROM events
                   WHERE ts = $2 AND key = $3 AND from_status = $4 AND to_status = $5
                     AND reason IS NOT DISTINCT FROM $6
                     AND evidence IS NOT DISTINCT FROM $7
                     AND actor IS NOT DISTINCT FROM $8)"#,
        )
        .bind(ev.v)
        .bind(&ev.ts)
        .bind(&ev.key)
        .bind(&ev.from)
        .bind(&ev.to)
        .bind(&ev.reason)
        .bind(&ev.evidence)
        .bind(&ev.actor)
        .execute(&mut *tx)
        .await
        .context("inserting an event line")?
        .rows_affected();
        if inserted > 0 {
            rep.migrated += 1;
        } else {
            rep.skipped += 1;
        }
    }
    tx.commit().await.context("commit events import")?;
    Ok(())
}

// --- autopilot.json ---------------------------------------------------------------------------

/// The old `autopilot.json` shape; `changed_at` maps to the row's `updated_at`.
#[derive(Debug, Deserialize)]
struct LegacyAutopilot {
    enabled: bool,
    #[serde(default)]
    changed_by: Option<String>,
    #[serde(default)]
    changed_at: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

async fn migrate_autopilot(
    pool: &PgPool,
    state_dir: &Path,
    rep: &mut CategoryReport,
) -> Result<()> {
    let path = state_dir.join("autopilot.json");
    if !path.is_file() {
        return Ok(());
    }
    let parsed: LegacyAutopilot = match std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))
        .and_then(|s| serde_json::from_str(&s).context("parsing autopilot.json"))
    {
        Ok(p) => p,
        Err(e) => {
            rep.failed.push(format!("{}: {e:#}", path.display()));
            return Ok(());
        }
    };
    let updated_at = parsed.changed_at.unwrap_or_else(crate::clock::now_rfc3339);
    let inserted = sqlx::query(
        r#"INSERT INTO autopilot (singleton, enabled, changed_by, reason, updated_at)
           VALUES (TRUE, $1, $2, $3, $4)
           ON CONFLICT (singleton) DO NOTHING"#,
    )
    .bind(parsed.enabled)
    .bind(&parsed.changed_by)
    .bind(&parsed.reason)
    .bind(&updated_at)
    .execute(pool)
    .await
    .context("inserting the autopilot row")?
    .rows_affected();
    if inserted > 0 {
        rep.migrated += 1;
    } else {
        rep.skipped += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pure parsing + tar determinism (no DB) ------------------------------------------------

    #[test]
    fn split_steer_without_markers_is_all_frozen() {
        let (frozen, blocks) = split_steer("guidance\nmore\n").expect("split");
        assert_eq!(frozen, "guidance\nmore\n");
        assert!(blocks.is_empty());
    }

    #[test]
    fn split_steer_separates_frozen_prefix_and_blocks() {
        let text = "frozen guidance\n\
                    <!-- steer @1755000000 by control -->\nhoist the dup check\n\
                    <!-- steer @1755000100 by control -->\nline one\nline two\n";
        let (frozen, blocks) = split_steer(text).expect("split");
        assert_eq!(frozen, "frozen guidance\n");
        assert_eq!(
            blocks,
            vec![
                SteerBlock {
                    epoch: 1755000000,
                    body: "hoist the dup check".to_string()
                },
                SteerBlock {
                    epoch: 1755000100,
                    body: "line one\nline two".to_string()
                },
            ]
        );
    }

    #[test]
    fn split_steer_with_no_frozen_prefix() {
        let text = "<!-- steer @1755000000 by control -->\nonly steering\n";
        let (frozen, blocks) = split_steer(text).expect("split");
        assert_eq!(frozen, "");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].body, "only steering");
    }

    #[test]
    fn split_steer_rejects_a_malformed_epoch() {
        let text = "frozen\n<!-- steer @not-a-number by control -->\nbody\n";
        assert!(split_steer(text).is_err());
        // A near-miss line that is not a marker at all stays in the frozen text.
        let (frozen, blocks) =
            split_steer("frozen\n<!-- steer by control -->\n").expect("not a marker");
        assert_eq!(frozen, "frozen\n<!-- steer by control -->\n");
        assert!(blocks.is_empty());
    }

    fn sample_pack_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("SCOPE.md"), "identity: v1:beef\n").unwrap();
        std::fs::create_dir_all(dir.path().join("gates")).unwrap();
        std::fs::write(dir.path().join("gates").join("judge.py"), "print(1)\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("HEAD"), "ref: nope\n").unwrap();
        dir
    }

    #[test]
    fn deterministic_tar_is_stable_across_mtime_churn() {
        let tree = sample_pack_tree();
        let first = deterministic_pack_tar(tree.path(), None).expect("tar");
        // Rewriting a file bumps its mtime; the tar bytes must not care.
        std::fs::write(tree.path().join("SCOPE.md"), "identity: v1:beef\n").unwrap();
        let second = deterministic_pack_tar(tree.path(), None).expect("tar");
        assert_eq!(first, second);
    }

    #[test]
    fn deterministic_tar_replaces_steer_and_excludes_git() {
        let tree = sample_pack_tree();
        std::fs::write(tree.path().join("STEER.md"), "frozen\nappended junk\n").unwrap();
        let tar_gz = deterministic_pack_tar(tree.path(), Some("frozen\n")).expect("tar");
        let scratch = tempfile::tempdir().expect("scratch");
        let out = scratch.path().join("pack");
        crate::playbooks::packs::unpack_pack_tgz(&tar_gz, &out).expect("unpack");
        assert_eq!(
            std::fs::read_to_string(out.join("STEER.md")).expect("STEER.md"),
            "frozen\n"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("gates").join("judge.py")).expect("nested"),
            "print(1)\n"
        );
        assert!(!out.join(".git").exists());
    }

    #[test]
    fn legacy_event_line_parses_with_and_without_actor() {
        let old = r#"{"v":1,"ts":"2026-07-02T12:34:56Z","key":"a/b#1","from":"new","to":"scoped","reason":null,"evidence":null}"#;
        let ev: LegacyEvent = serde_json::from_str(old).expect("pre-actor line");
        assert_eq!(ev.key, "a/b#1");
        assert!(ev.actor.is_none());
        let with = r#"{"v":1,"ts":"2026-07-02T12:34:56Z","key":"a/b#1","from":"audit","to":"audit","reason":"repo watch flip","evidence":null,"actor":"wren"}"#;
        let ev: LegacyEvent = serde_json::from_str(with).expect("actor line");
        assert_eq!(ev.actor.as_deref(), Some("wren"));
        assert!(serde_json::from_str::<LegacyEvent>("not json").is_err());
    }

    // --- DB-backed fixture runs ----------------------------------------------------------------

    const STEER_FILE: &str = "frozen guidance\n\
        <!-- steer @1755000000 by control -->\nhoist the dup check\n\
        <!-- steer @1755000100 by control -->\npick fail-closed\n";

    fn fixture_state_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("evidence").join("pod-a")).unwrap();
        std::fs::write(
            root.join("evidence").join("pod-a").join("scope-pack"),
            b"gz-pack-bytes",
        )
        .unwrap();
        std::fs::write(
            root.join("evidence")
                .join("pod-a")
                .join(".scope-pack.123.tmp"),
            b"torn upload leftovers",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("evidence").join("external").join("run-ext")).unwrap();
        std::fs::write(
            root.join("evidence")
                .join("external")
                .join("run-ext")
                .join("session.jsonl"),
            "{\"v\":1,\"kind\":\"shutdown\"}\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("runs").join("run-1")).unwrap();
        std::fs::write(
            root.join("runs").join("run-1").join("session.jsonl"),
            "{\"v\":1,\"kind\":\"turn\"}\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("adopted").join("run-2")).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, b"{\"v\":1,\"kind\":\"adopted\"}\n").unwrap();
        std::fs::write(
            root.join("adopted").join("run-2").join("session.jsonl"),
            gz.finish().unwrap(),
        )
        .unwrap();
        let pack = root.join("packs").join("owner_repo_7");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        std::fs::write(pack.join("SCOPE.md"), "identity: v1:beef\n").unwrap();
        std::fs::write(pack.join("STEER.md"), STEER_FILE).unwrap();
        std::fs::write(
            root.join("controller-events.jsonl"),
            concat!(
                r#"{"v":1,"ts":"2026-07-02T12:34:56Z","key":"a/b#1","from":"new","to":"scoped","reason":null,"evidence":null}"#,
                "\n",
                r#"{"v":1,"ts":"2026-07-02T12:34:56Z","key":"a/b#1","from":"new","to":"scoped","reason":null,"evidence":null}"#,
                "\n",
                r#"{"v":1,"ts":"2026-07-02T12:35:00Z","key":"a/b#1","from":"scoped","to":"parked","reason":"no repro","evidence":null,"actor":"wren"}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("autopilot.json"),
            r#"{"enabled":false,"changed_by":"wren","changed_at":"2026-08-01T00:00:00Z","reason":"cost runaway"}"#,
        )
        .unwrap();
        dir
    }

    async fn seed_run_row(pool: &PgPool, run_id: &str, session_uri: Option<&str>) {
        sqlx::query("INSERT INTO runs (run_id, status, session_uri) VALUES ($1, 'done', $2)")
            .bind(run_id)
            .bind(session_uri)
            .execute(pool)
            .await
            .expect("seed run row");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn migrates_a_full_fixture_and_is_idempotent(pool: PgPool) {
        let state = fixture_state_dir();
        crate::runs::work_pods::record_pod_artifact(
            &pool,
            &crate::runs::model::NewPodArtifact {
                pod: "pod-a".to_string(),
                kind: "scope-pack".to_string(),
                digest: content_digest(b"gz-pack-bytes"),
                bytes: b"gz-pack-bytes".len() as i64,
            },
        )
        .await
        .expect("seed pointer");
        seed_run_row(&pool, "run-1", Some("/state/runs/run-1/session.jsonl")).await;
        seed_run_row(&pool, "run-s3", Some("s3://bucket/x/session.jsonl")).await;

        let report = migrate_state(&pool, state.path(), false)
            .await
            .expect("migrate");
        assert!(report.failures().is_empty(), "{:?}", report.failures());
        assert_eq!(report.evidence.migrated, 1);
        assert_eq!(report.sessions.migrated, 3);
        assert_eq!(report.packs.migrated, 1);
        assert_eq!(report.steering.migrated, 2);
        assert_eq!((report.events.migrated, report.events.skipped), (2, 1));
        assert_eq!(report.autopilot.migrated, 1);

        let owner = crate::runs::blob_store::ArtifactOwner::PodEvidence {
            pod: "pod-a".to_string(),
        };
        let held = crate::runs::blob_store::get_artifact(&pool, &owner, "scope-pack")
            .await
            .expect("get")
            .expect("stored");
        assert_eq!(held.data, b"gz-pack-bytes");

        for (run_id, body) in [
            ("run-1", "{\"v\":1,\"kind\":\"turn\"}\n"),
            ("run-2", "{\"v\":1,\"kind\":\"adopted\"}\n"),
            ("run-ext", "{\"v\":1,\"kind\":\"shutdown\"}\n"),
        ] {
            assert_eq!(
                crate::runs::blob_store::get_run_session(&pool, run_id)
                    .await
                    .expect("get"),
                Some(body.to_string()),
                "session for {run_id}"
            );
        }
        let uri: Option<String> =
            sqlx::query_scalar("SELECT session_uri FROM runs WHERE run_id = 'run-1'")
                .fetch_one(&pool)
                .await
                .expect("uri");
        assert_eq!(
            uri.as_deref(),
            Some("db://run-session/run-1/session.jsonl"),
            "local session_uri repointed at the store"
        );
        let s3: Option<String> =
            sqlx::query_scalar("SELECT session_uri FROM runs WHERE run_id = 'run-s3'")
                .fetch_one(&pool)
                .await
                .expect("uri");
        assert_eq!(s3.as_deref(), Some("s3://bucket/x/session.jsonl"));

        // Materializing the migrated pack reconstructs the original STEER.md byte-for-byte:
        // frozen prefix from the tarball, blocks re-injected from the rows.
        let pack = crate::playbooks::packs::materialize_pack(&pool, "owner/repo#7")
            .await
            .expect("materialize")
            .expect("stored");
        assert_eq!(
            std::fs::read_to_string(pack.path().join("STEER.md")).expect("STEER.md"),
            STEER_FILE
        );

        let events = crate::event_log::EventLog::new(pool.clone())
            .read_all()
            .await
            .expect("events");
        assert_eq!(events.len(), 2, "the duplicate line collapsed");
        assert_eq!(events[1].actor.as_deref(), Some("wren"));

        let flag = crate::runs::blob_store::get_autopilot(&pool)
            .await
            .expect("get")
            .expect("row");
        assert!(!flag.enabled);
        assert_eq!(flag.updated_at, "2026-08-01T00:00:00Z");

        // Second run: everything short-circuits, nothing double-lands.
        let again = migrate_state(&pool, state.path(), false)
            .await
            .expect("re-run");
        assert!(again.failures().is_empty(), "{:?}", again.failures());
        for (name, cat) in again.categories() {
            assert_eq!(cat.migrated, 0, "{name} re-migrated");
        }
        assert_eq!(again.evidence.skipped, 1);
        assert_eq!(again.sessions.skipped, 3);
        assert_eq!(again.packs.skipped, 1);
        assert_eq!(again.steering.skipped, 2);
        assert_eq!(again.events.skipped, 3);
        assert_eq!(again.autopilot.skipped, 1);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(n, 2);
        let steer_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pack_steering")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(steer_rows, 2);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn evidence_digest_mismatch_is_a_listed_failure(pool: PgPool) {
        let state = tempfile::tempdir().expect("tempdir");
        let dir = state.path().join("evidence").join("pod-a");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("otel-log"), b"actual bytes").unwrap();
        crate::runs::work_pods::record_pod_artifact(
            &pool,
            &crate::runs::model::NewPodArtifact {
                pod: "pod-a".to_string(),
                kind: "otel-log".to_string(),
                digest: content_digest(b"different bytes"),
                bytes: 42,
            },
        )
        .await
        .expect("seed pointer");

        let report = migrate_state(&pool, state.path(), false)
            .await
            .expect("migrate");
        assert_eq!(report.evidence.migrated, 0);
        assert_eq!(report.evidence.failed.len(), 1);
        assert!(
            report.evidence.failed[0].contains("otel-log")
                && report.evidence.failed[0].contains("digest mismatch"),
            "{:?}",
            report.evidence.failed
        );
        let owner = crate::runs::blob_store::ArtifactOwner::PodEvidence {
            pod: "pod-a".to_string(),
        };
        assert!(
            crate::runs::blob_store::get_artifact(&pool, &owner, "otel-log")
                .await
                .expect("get")
                .is_none(),
            "a mismatched file must not land"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn bad_event_lines_fail_unless_skipped(pool: PgPool) {
        let state = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            state.path().join("controller-events.jsonl"),
            concat!(
                r#"{"v":1,"ts":"2026-07-02T12:34:56Z","key":"a/b#1","from":"new","to":"scoped","reason":null,"evidence":null}"#,
                "\n",
                "not json at all\n",
            ),
        )
        .unwrap();

        let report = migrate_state(&pool, state.path(), false)
            .await
            .expect("migrate");
        assert_eq!(report.events.bad_lines, 1);
        assert_eq!(report.events.migrated, 0, "nothing imports on a bad file");
        assert_eq!(report.events.failed.len(), 1);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(n, 0);

        let report = migrate_state(&pool, state.path(), true)
            .await
            .expect("migrate with skip");
        assert_eq!(report.events.bad_lines, 1);
        assert_eq!(report.events.migrated, 1);
        assert!(report.events.failed.is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_stored_pack_with_a_different_digest_is_a_failure(pool: PgPool) {
        let state = tempfile::tempdir().expect("tempdir");
        let pack = state.path().join("packs").join("owner_repo_7");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        crate::runs::blob_store::put_pack_tarball(&pool, "owner_repo_7", b"some other tarball")
            .await
            .expect("seed");

        let report = migrate_state(&pool, state.path(), false)
            .await
            .expect("migrate");
        assert_eq!(report.packs.migrated, 0);
        assert_eq!(report.packs.failed.len(), 1);
        assert!(
            report.packs.failed[0].contains("owner_repo_7"),
            "{:?}",
            report.packs.failed
        );
    }

    /// The state a non-transactional import could have left behind: the tarball landed, the
    /// steering rows did not. A re-run must import the rows, not report skipped-existing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_pack_with_a_stored_tarball_but_no_steering_rows_heals(pool: PgPool) {
        let state = tempfile::tempdir().expect("tempdir");
        let pack = state.path().join("packs").join("owner_repo_7");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        std::fs::write(pack.join("STEER.md"), STEER_FILE).unwrap();
        let (frozen, _) = split_steer(STEER_FILE).expect("split");
        let tar_gz = deterministic_pack_tar(&pack, Some(&frozen)).expect("tar");
        crate::runs::blob_store::put_pack_tarball(&pool, "owner_repo_7", &tar_gz)
            .await
            .expect("seed the tarball without its rows");

        let report = migrate_state(&pool, state.path(), false)
            .await
            .expect("migrate");
        assert!(report.failures().is_empty(), "{:?}", report.failures());
        assert_eq!(report.packs.skipped, 1, "digest matched");
        assert_eq!(report.steering.migrated, 2, "the missing rows imported");
        let materialized = crate::playbooks::packs::materialize_pack(&pool, "owner/repo#7")
            .await
            .expect("materialize")
            .expect("stored");
        assert_eq!(
            std::fs::read_to_string(materialized.path().join("STEER.md")).expect("STEER.md"),
            STEER_FILE
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_failed_pack_import_lands_neither_tarball_nor_rows(pool: PgPool) {
        let state = tempfile::tempdir().expect("tempdir");
        let pack = state.path().join("packs").join("owner_repo_8");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        // Parses as i64, overflows jiff's Timestamp range — fails after the split, before any write.
        std::fs::write(
            pack.join("STEER.md"),
            "frozen\n<!-- steer @9223372036854775807 by control -->\nbody\n",
        )
        .unwrap();

        let report = migrate_state(&pool, state.path(), false)
            .await
            .expect("migrate");
        assert_eq!(report.packs.failed.len(), 1, "{:?}", report.packs.failed);
        assert_eq!(report.packs.migrated, 0);
        assert_eq!(report.steering.migrated, 0);
        let tarballs: i64 = sqlx::query_scalar("SELECT count(*) FROM pack_tarballs")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(tarballs, 0, "a failed pack must land nothing");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pack_steering")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn refuses_to_run_while_the_maintenance_lock_is_held(pool: PgPool) {
        let mut holder = pool.acquire().await.expect("holder connection");
        let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(crate::client::MAINTENANCE_ADVISORY_LOCK)
            .fetch_one(&mut *holder)
            .await
            .expect("hold the lock");
        assert!(held);

        let state = tempfile::tempdir().expect("tempdir");
        let err = migrate_state(&pool, state.path(), false)
            .await
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("maintenance advisory lock"),
            "{err:#}"
        );

        sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(crate::client::MAINTENANCE_ADVISORY_LOCK)
            .fetch_one(&mut *holder)
            .await
            .expect("release");
        migrate_state(&pool, state.path(), false)
            .await
            .expect("runs once released");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_missing_state_dir_is_an_error_not_an_empty_success(pool: PgPool) {
        let err = migrate_state(&pool, Path::new("/nonexistent/state"), false)
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("does not exist"), "{err:#}");
    }

    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unsplittable_steer_md_tars_as_is_with_no_rows(pool: PgPool) {
        let state = tempfile::tempdir().expect("tempdir");
        let pack = state.path().join("packs").join("owner_repo_9");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        let steer = "frozen\n<!-- steer @garbage by control -->\nbody\n";
        std::fs::write(pack.join("STEER.md"), steer).unwrap();

        let report = migrate_state(&pool, state.path(), false)
            .await
            .expect("migrate");
        assert!(report.failures().is_empty(), "{:?}", report.failures());
        assert_eq!(report.packs.migrated, 1);
        assert_eq!(report.steering.migrated, 0);
        assert_eq!(
            crate::playbooks::packs::read_pack_file(&pool, "owner/repo#9", "STEER.md")
                .await
                .expect("read"),
            Some(steer.to_string()),
            "the whole file rode the tarball"
        );
    }
}

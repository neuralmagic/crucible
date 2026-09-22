//! Payload storage over Postgres (migration `0004_shared_state.sql`): artifact bodies as ordered
//! gzipped chunks, pack tarballs, steering appends, and the autopilot flag. This module owns the
//! bytes; the existing `pod_artifacts` pointer rows stay where they are.
//!
//! [`put_artifact`] is streaming-friendly: it consumes the body piece by piece, hashes inline,
//! enforces the caller's cap mid-stream, and lands every chunk in one transaction, so a torn or
//! oversize upload leaves nothing behind. [`get_artifact`] concatenates the chunks and verifies
//! the digest before handing the payload back.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, ensure};
use crucible_contract::content_digest;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::fmt::Write as _;

/// Chunk size for `artifact_chunks.data`: large enough to keep row count low, small enough to
/// bound per-row TOAST and WAL cost.
pub const ARTIFACT_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// The `artifacts.kind` a pod run's own output (the engine's tracing, gateway boot, wrapper
/// chatter) is stored under at completion, so it outlives the pod (owner [`ArtifactOwner::Run`]).
pub const RUN_ENGINE_LOG_KIND: &str = "run-engine-log";

/// How much of a pod's engine output is kept, counted from the end. The gateway log tails and
/// agent stderr that explain a failure are at the end; a 500MB stdout is not evidence.
const RUN_ENGINE_LOG_KEEP: usize = 8 * 1024 * 1024;

/// The `runs.session_uri` scheme prefix for a session stored in the artifact tables.
pub const DB_SESSION_URI_PREFIX: &str = "db://run-session/";

/// The `session_uri` recorded for a store-backed run session. Ends in `session.jsonl` so the
/// artifact proxy's `prefix_of` resolves siblings off it like any other pointer.
pub fn run_session_uri(run_id: &str) -> String {
    format!("{DB_SESSION_URI_PREFIX}{run_id}/session.jsonl")
}

/// The run id inside a `db://run-session/…` uri or prefix, or `None` for any other scheme.
pub fn run_id_of_session_uri(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix(DB_SESSION_URI_PREFIX)?;
    let run_id = rest.split('/').next().unwrap_or(rest);
    (!run_id.is_empty()).then_some(run_id)
}

/// Whose payload an `artifacts` row holds. The variants mirror the `owner_kind` CHECK constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactOwner {
    /// A drop-box body a work pod POSTed; `pod` is the uploading pod's name.
    PodEvidence { pod: String },
    /// An artifact a run owns, dispatched or adopted; `run_id` is the ledger's run id.
    Run { run_id: String },
}

impl ArtifactOwner {
    fn owner_kind(&self) -> &'static str {
        match self {
            ArtifactOwner::PodEvidence { .. } => "pod-evidence",
            ArtifactOwner::Run { .. } => "run-session",
        }
    }

    fn owner_id(&self) -> &str {
        match self {
            ArtifactOwner::PodEvidence { pod } => pod,
            ArtifactOwner::Run { run_id } => run_id,
        }
    }
}

/// What [`put_artifact`] recorded: the row id and the digest/size it computed off the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtifact {
    pub id: i64,
    pub digest: String,
    pub bytes: u64,
}

/// A payload read back whole, digest-verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPayload {
    pub data: Vec<u8>,
    pub digest: String,
}

/// One `pack_steering` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringEntry {
    pub seq: i64,
    pub body_md: String,
    pub author: Option<String>,
    pub created_at: String,
}

/// The `autopilot` singleton row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutopilotRow {
    pub enabled: bool,
    pub changed_by: Option<String>,
    pub reason: Option<String>,
    pub updated_at: String,
}

/// Why [`put_artifact`] refused or failed, split so the ingest approval can map cap violations to 413
/// and stream errors to 400 without string-matching.
#[derive(Debug, thiserror::Error)]
pub enum PutArtifactError {
    #[error("payload exceeds the cap of {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("reading the payload stream: {0}")]
    Source(String),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// Store one artifact payload from a stream of byte pieces. Replaces any prior payload for the
/// same `(owner, kind)`. Everything happens in one transaction: an error (including the cap)
/// rolls the whole write back.
pub async fn put_artifact<S, B, E>(
    pool: &PgPool,
    owner: &ArtifactOwner,
    kind: &str,
    max_bytes: u64,
    mut stream: S,
) -> Result<StoredArtifact, PutArtifactError>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut tx = pool.begin().await.context("begin put_artifact")?;
    sqlx::query("DELETE FROM artifacts WHERE owner_kind = $1 AND owner_id = $2 AND kind = $3")
        .bind(owner.owner_kind())
        .bind(owner.owner_id())
        .bind(kind)
        .execute(&mut *tx)
        .await
        .context("clearing prior artifact")?;
    let row = sqlx::query(
        r#"INSERT INTO artifacts (owner_kind, owner_id, kind, digest, bytes, created_at)
           VALUES ($1, $2, $3, '', 0, $4) RETURNING id"#,
    )
    .bind(owner.owner_kind())
    .bind(owner.owner_id())
    .bind(kind)
    .bind(crate::clock::now_rfc3339())
    .fetch_one(&mut *tx)
    .await
    .context("inserting artifact row")?;
    let id: i64 = row.get("id");

    let mut chunker = Chunker::new(ARTIFACT_CHUNK_BYTES);
    let mut seq: i64 = 0;
    while let Some(piece) = stream.next().await {
        let piece = piece.map_err(|e| PutArtifactError::Source(e.to_string()))?;
        let piece = piece.as_ref();
        if chunker.total() + piece.len() as u64 > max_bytes {
            return Err(PutArtifactError::TooLarge { limit: max_bytes });
        }
        for chunk in chunker.push(piece) {
            insert_chunk(&mut tx, id, seq, &chunk).await?;
            seq += 1;
        }
    }
    let (tail, digest, bytes) = chunker.finish();
    if let Some(chunk) = tail {
        insert_chunk(&mut tx, id, seq, &chunk).await?;
    }

    sqlx::query("UPDATE artifacts SET digest = $1, bytes = $2 WHERE id = $3")
        .bind(&digest)
        .bind(i64::try_from(bytes).context("artifact byte count exceeds i64")?)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("finalizing artifact row")?;
    tx.commit().await.context("commit put_artifact")?;
    Ok(StoredArtifact { id, digest, bytes })
}

async fn insert_chunk(
    tx: &mut Transaction<'_, Postgres>,
    artifact_id: i64,
    seq: i64,
    data: &[u8],
) -> Result<()> {
    sqlx::query("INSERT INTO artifact_chunks (artifact_id, seq, data) VALUES ($1, $2, $3)")
        .bind(artifact_id)
        .bind(seq)
        .bind(data)
        .execute(&mut **tx)
        .await
        .context("inserting artifact chunk")?;
    Ok(())
}

/// Read one artifact payload back whole, or `None` if the `(owner, kind)` was never stored.
/// Errs if the concatenated chunks disagree with the recorded size or digest.
pub async fn get_artifact(
    pool: &PgPool,
    owner: &ArtifactOwner,
    kind: &str,
) -> Result<Option<ArtifactPayload>> {
    let row = sqlx::query(
        r#"SELECT id, digest, bytes FROM artifacts
           WHERE owner_kind = $1 AND owner_id = $2 AND kind = $3"#,
    )
    .bind(owner.owner_kind())
    .bind(owner.owner_id())
    .bind(kind)
    .fetch_optional(pool)
    .await
    .context("looking up artifact")?;
    let Some(row) = row else { return Ok(None) };
    let id: i64 = row.get("id");
    let digest: String = row.get("digest");
    let bytes: i64 = row.get("bytes");

    let chunks =
        sqlx::query("SELECT data FROM artifact_chunks WHERE artifact_id = $1 ORDER BY seq")
            .bind(id)
            .fetch_all(pool)
            .await
            .context("reading artifact chunks")?;
    let mut data = Vec::with_capacity(usize::try_from(bytes).unwrap_or(0));
    for chunk in chunks {
        data.extend_from_slice(&chunk.get::<Vec<u8>, _>("data"));
    }
    ensure!(
        data.len() as i64 == bytes,
        "artifact {}/{}/{kind}: {} chunk bytes but the row records {bytes}",
        owner.owner_kind(),
        owner.owner_id(),
        data.len(),
    );
    let got = content_digest(&data);
    ensure!(
        got == digest,
        "artifact {}/{}/{kind}: digest mismatch, stored {digest}, read {got}",
        owner.owner_kind(),
        owner.owner_id(),
    );
    Ok(Some(ArtifactPayload { data, digest }))
}

/// The recorded digest for one `(owner, kind)` without reading the chunks, or `None` if the
/// artifact was never stored.
pub(crate) async fn artifact_digest(
    pool: &PgPool,
    owner: &ArtifactOwner,
    kind: &str,
) -> Result<Option<String>> {
    sqlx::query_scalar(
        r#"SELECT digest FROM artifacts
           WHERE owner_kind = $1 AND owner_id = $2 AND kind = $3"#,
    )
    .bind(owner.owner_kind())
    .bind(owner.owner_id())
    .bind(kind)
    .fetch_optional(pool)
    .await
    .context("looking up artifact digest")
}

/// [`put_artifact`] over an in-memory payload.
pub async fn put_artifact_bytes(
    pool: &PgPool,
    owner: &ArtifactOwner,
    kind: &str,
    max_bytes: u64,
    data: Vec<u8>,
) -> Result<StoredArtifact, PutArtifactError> {
    let stream = futures_util::stream::iter([Ok::<Vec<u8>, std::convert::Infallible>(data)]);
    put_artifact(pool, owner, kind, max_bytes, stream).await
}

/// Store a run's session NDJSON (gzipped at rest, like every ingested artifact). Replaces any
/// prior session for the run.
pub async fn put_run_session(pool: &PgPool, run_id: &str, ndjson: &[u8]) -> Result<StoredArtifact> {
    use std::io::Write as _;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(ndjson).context("gzip run session")?;
    let gz = enc.finish().context("gzip run session")?;
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    let cap = crucible_contract::ArtifactKind::RunSession.max_bytes();
    put_artifact_bytes(
        pool,
        &owner,
        crucible_contract::ArtifactKind::RunSession.as_str(),
        cap,
        gz,
    )
    .await
    .map_err(anyhow::Error::from)
}

/// Store a pod run's own output, gzipped at rest, keeping the last [`RUN_ENGINE_LOG_KEEP`] bytes
/// on a line boundary. Replaces any prior copy for the run.
pub async fn put_run_engine_log(pool: &PgPool, run_id: &str, text: &str) -> Result<StoredArtifact> {
    use std::io::Write as _;
    let kept = engine_log_tail(text);
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(kept.as_bytes())
        .context("gzip run engine log")?;
    let gz = enc.finish().context("gzip run engine log")?;
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    put_artifact_bytes(pool, &owner, RUN_ENGINE_LOG_KIND, u64::MAX, gz)
        .await
        .map_err(anyhow::Error::from)
}

/// The last [`RUN_ENGINE_LOG_KEEP`] bytes of `text`, starting on a line boundary.
fn engine_log_tail(text: &str) -> &str {
    if text.len() <= RUN_ENGINE_LOG_KEEP {
        return text;
    }
    let start = text.len() - RUN_ENGINE_LOG_KEEP;
    match text[start..].find('\n') {
        Some(nl) => &text[start + nl + 1..],
        None => &text[start..],
    }
}

/// A pod run's stored engine output, or `None` for a run that finished before it was kept.
pub async fn get_run_engine_log(pool: &PgPool, run_id: &str) -> Result<Option<String>> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    let Some(payload) = get_artifact(pool, &owner, RUN_ENGINE_LOG_KIND).await? else {
        return Ok(None);
    };
    Ok(Some(gunzip_maybe(&payload.data)?))
}

/// Store a run's captured files: the gzipped tar the loop POSTed, kept exactly as ingested.
/// Replaces any prior bundle for the run.
pub async fn put_run_files(pool: &PgPool, run_id: &str, tar_gz: Vec<u8>) -> Result<StoredArtifact> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    let cap = crucible_contract::ArtifactKind::RunFiles.max_bytes();
    put_artifact_bytes(
        pool,
        &owner,
        crucible_contract::ArtifactKind::RunFiles.as_str(),
        cap,
        tar_gz,
    )
    .await
    .map_err(anyhow::Error::from)
}

/// Read a run's captured-files bundle back as the gzipped tar bytes, or `None` if never stored.
pub async fn get_run_files(pool: &PgPool, run_id: &str) -> Result<Option<Vec<u8>>> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    Ok(get_artifact(
        pool,
        &owner,
        crucible_contract::ArtifactKind::RunFiles.as_str(),
    )
    .await?
    .map(|payload| payload.data))
}

/// Read a run's stored session back as NDJSON text, or `None` if never stored.
pub async fn get_run_session(pool: &PgPool, run_id: &str) -> Result<Option<String>> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    let Some(payload) = get_artifact(
        pool,
        &owner,
        crucible_contract::ArtifactKind::RunSession.as_str(),
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(gunzip_maybe(&payload.data)?))
}

/// Gunzip a payload; a non-gzip body is returned as UTF-8 as-is.
pub fn gunzip_maybe(raw: &[u8]) -> Result<String> {
    use std::io::Read as _;
    if raw.starts_with(&[0x1f, 0x8b]) {
        let mut d = flate2::read::MultiGzDecoder::new(raw);
        let mut s = String::new();
        d.read_to_string(&mut s).context("gunzip payload")?;
        Ok(s)
    } else {
        Ok(String::from_utf8_lossy(raw).into_owned())
    }
}

/// Store (or replace) the frozen pack tarball for one sanitized issue key. Returns the digest.
/// Generic over the executor so the write can join a caller's transaction.
pub async fn put_pack_tarball(
    ex: impl sqlx::PgExecutor<'_>,
    issue_slug: &str,
    tar_gz: &[u8],
) -> Result<String> {
    let digest = content_digest(tar_gz);
    sqlx::query(
        r#"INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (issue_slug) DO UPDATE SET
               tar_gz = excluded.tar_gz, digest = excluded.digest, bytes = excluded.bytes,
               created_at = excluded.created_at"#,
    )
    .bind(issue_slug)
    .bind(tar_gz)
    .bind(&digest)
    .bind(tar_gz.len() as i64)
    .bind(crate::clock::now_rfc3339())
    .execute(ex)
    .await
    .context("storing pack tarball")?;
    Ok(digest)
}

/// Read one pack tarball back, digest-verified, or `None` if never stored.
pub async fn get_pack_tarball(pool: &PgPool, issue_slug: &str) -> Result<Option<Vec<u8>>> {
    let row = sqlx::query("SELECT tar_gz, digest FROM pack_tarballs WHERE issue_slug = $1")
        .bind(issue_slug)
        .fetch_optional(pool)
        .await
        .context("reading pack tarball")?;
    let Some(row) = row else { return Ok(None) };
    let tar_gz: Vec<u8> = row.get("tar_gz");
    let digest: String = row.get("digest");
    let got = content_digest(&tar_gz);
    ensure!(
        got == digest,
        "pack {issue_slug}: digest mismatch, stored {digest}, read {got}"
    );
    Ok(Some(tar_gz))
}

/// Append one steering entry for an issue's pack. Returns the assigned seq (1-based).
pub async fn append_steering(
    pool: &PgPool,
    issue_slug: &str,
    body_md: &str,
    author: Option<&str>,
) -> Result<i64> {
    let row = sqlx::query(
        r#"INSERT INTO pack_steering (issue_slug, seq, body_md, author, created_at)
           SELECT $1, COALESCE(MAX(seq), 0) + 1, $2, $3, $4
           FROM pack_steering WHERE issue_slug = $1
           RETURNING seq"#,
    )
    .bind(issue_slug)
    .bind(body_md)
    .bind(author)
    .bind(crate::clock::now_rfc3339())
    .fetch_one(pool)
    .await
    .context("appending steering")?;
    Ok(row.get("seq"))
}

/// Every steering entry for an issue's pack, oldest first.
pub async fn list_steering(pool: &PgPool, issue_slug: &str) -> Result<Vec<SteeringEntry>> {
    let rows = sqlx::query(
        r#"SELECT seq, body_md, author, created_at FROM pack_steering
           WHERE issue_slug = $1 ORDER BY seq"#,
    )
    .bind(issue_slug)
    .fetch_all(pool)
    .await
    .context("listing steering")?;
    Ok(rows
        .into_iter()
        .map(|r| SteeringEntry {
            seq: r.get("seq"),
            body_md: r.get("body_md"),
            author: r.get("author"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// The autopilot flag row, or `None` if it was never set.
pub async fn get_autopilot(pool: &PgPool) -> Result<Option<AutopilotRow>> {
    let row = sqlx::query("SELECT enabled, changed_by, reason, updated_at FROM autopilot")
        .fetch_optional(pool)
        .await
        .context("reading autopilot flag")?;
    Ok(row.map(|r| AutopilotRow {
        enabled: r.get("enabled"),
        changed_by: r.get("changed_by"),
        reason: r.get("reason"),
        updated_at: r.get("updated_at"),
    }))
}

/// Set the autopilot flag (upserting the singleton row). Returns the row as written.
pub async fn set_autopilot(
    pool: &PgPool,
    enabled: bool,
    changed_by: Option<&str>,
    reason: Option<&str>,
) -> Result<AutopilotRow> {
    let updated_at = crate::clock::now_rfc3339();
    sqlx::query(
        r#"INSERT INTO autopilot (singleton, enabled, changed_by, reason, updated_at)
           VALUES (TRUE, $1, $2, $3, $4)
           ON CONFLICT (singleton) DO UPDATE SET
               enabled = excluded.enabled, changed_by = excluded.changed_by,
               reason = excluded.reason, updated_at = excluded.updated_at"#,
    )
    .bind(enabled)
    .bind(changed_by)
    .bind(reason)
    .bind(&updated_at)
    .execute(pool)
    .await
    .context("setting autopilot flag")?;
    Ok(AutopilotRow {
        enabled,
        changed_by: changed_by.map(str::to_string),
        reason: reason.map(str::to_string),
        updated_at,
    })
}

/// Fixed-size splitter with an inline hasher: feed arbitrary pieces, get back full chunks as they
/// fill, and the tail + `sha256:<hex>` digest + total byte count at the end.
struct Chunker {
    chunk_bytes: usize,
    buf: Vec<u8>,
    hasher: Sha256,
    total: u64,
}

impl Chunker {
    fn new(chunk_bytes: usize) -> Self {
        Chunker {
            chunk_bytes,
            buf: Vec::new(),
            hasher: Sha256::new(),
            total: 0,
        }
    }

    fn total(&self) -> u64 {
        self.total
    }

    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.hasher.update(data);
        self.total += data.len() as u64;
        self.buf.extend_from_slice(data);
        let mut full = Vec::new();
        while self.buf.len() >= self.chunk_bytes {
            let rest = self.buf.split_off(self.chunk_bytes);
            full.push(std::mem::replace(&mut self.buf, rest));
        }
        full
    }

    fn finish(self) -> (Option<Vec<u8>>, String, u64) {
        let hash = self.hasher.finalize();
        let mut digest = String::with_capacity("sha256:".len() + hash.len() * 2);
        digest.push_str("sha256:");
        for b in hash {
            let _ = write!(digest, "{b:02x}");
        }
        let tail = (!self.buf.is_empty()).then_some(self.buf);
        (tail, digest, self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[test]
    fn the_kept_engine_log_is_the_tail_on_a_line_boundary() {
        let short = "a\nb\n";
        assert_eq!(engine_log_tail(short), short);
        let line = "x".repeat(1024) + "\n";
        let long: String = std::iter::repeat_n(line.as_str(), 9 * 1024).collect();
        let kept = engine_log_tail(&long);
        assert!(kept.len() <= RUN_ENGINE_LOG_KEEP);
        assert!(kept.starts_with('x') && kept.ends_with('\n'));
        assert_eq!(kept.len() % line.len(), 0, "cut on a line boundary");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_engine_log_round_trips_and_replaces(pool: sqlx::PgPool) {
        assert_eq!(get_run_engine_log(&pool, "run-x").await.unwrap(), None);
        put_run_engine_log(&pool, "run-x", "first\n").await.unwrap();
        put_run_engine_log(&pool, "run-x", "gateway_boot: ok\nagent: exit 0\n")
            .await
            .unwrap();
        assert_eq!(
            get_run_engine_log(&pool, "run-x").await.unwrap().as_deref(),
            Some("gateway_boot: ok\nagent: exit 0\n")
        );
    }

    fn feed(chunker: &mut Chunker, data: &[u8], piece: usize) -> Vec<Vec<u8>> {
        data.chunks(piece.max(1))
            .flat_map(|p| chunker.push(p))
            .collect()
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn chunker_splits_and_rejoins_across_piece_boundaries() {
        let chunk = 8;
        let data = payload(3 * chunk + 5);
        for piece in [1, 3, chunk, chunk + 1, data.len()] {
            let mut chunker = Chunker::new(chunk);
            let mut full = feed(&mut chunker, &data, piece);
            let (tail, digest, total) = chunker.finish();
            assert!(full.iter().all(|c| c.len() == chunk), "piece={piece}");
            assert_eq!(full.len(), 3, "piece={piece}");
            let tail = tail.expect("5 trailing bytes");
            assert_eq!(tail.len(), 5);
            full.push(tail);
            assert_eq!(full.concat(), data, "piece={piece}");
            assert_eq!(total, data.len() as u64);
            assert_eq!(digest, content_digest(&data), "piece={piece}");
        }
    }

    #[test]
    fn chunker_exact_multiple_leaves_no_tail() {
        let chunk = 8;
        let data = payload(2 * chunk);
        let mut chunker = Chunker::new(chunk);
        let full = chunker.push(&data);
        let (tail, digest, total) = chunker.finish();
        assert_eq!(full.len(), 2);
        assert!(tail.is_none());
        assert_eq!(total, data.len() as u64);
        assert_eq!(digest, content_digest(&data));
    }

    #[test]
    fn chunker_empty_input_digests_the_empty_payload() {
        let chunker = Chunker::new(8);
        let (tail, digest, total) = chunker.finish();
        assert!(tail.is_none());
        assert_eq!(total, 0);
        assert_eq!(digest, content_digest(b""));
    }

    fn ok_stream(
        data: Vec<u8>,
        piece: usize,
    ) -> impl Stream<Item = Result<Vec<u8>, String>> + Unpin {
        let pieces: Vec<Result<Vec<u8>, String>> =
            data.chunks(piece.max(1)).map(|p| Ok(p.to_vec())).collect();
        stream::iter(pieces)
    }

    fn pod_owner() -> ArtifactOwner {
        ArtifactOwner::PodEvidence {
            pod: "crucible-turn-x".to_string(),
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn artifact_roundtrips_across_multiple_chunks(pool: sqlx::PgPool) {
        // Big enough for two full chunks plus a tail, streamed in awkward piece sizes.
        let data = payload(2 * ARTIFACT_CHUNK_BYTES + 12345);
        let owner = pod_owner();
        let stored = put_artifact(
            &pool,
            &owner,
            "run-session",
            data.len() as u64,
            ok_stream(data.clone(), 1_000_003),
        )
        .await
        .expect("put");
        assert_eq!(stored.bytes, data.len() as u64);
        assert_eq!(stored.digest, content_digest(&data));

        let back = get_artifact(&pool, &owner, "run-session")
            .await
            .expect("get")
            .expect("stored");
        assert_eq!(back.data, data);
        assert_eq!(back.digest, stored.digest);

        let chunk_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM artifact_chunks WHERE artifact_id = $1")
                .bind(stored.id)
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(chunk_count, 3);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn put_replaces_a_prior_payload_for_the_same_owner_and_kind(pool: sqlx::PgPool) {
        let owner = pod_owner();
        let first = payload(ARTIFACT_CHUNK_BYTES + 7);
        put_artifact(
            &pool,
            &owner,
            "scope-pack",
            u64::MAX,
            ok_stream(first, 4096),
        )
        .await
        .expect("first put");
        let second = payload(99);
        put_artifact(
            &pool,
            &owner,
            "scope-pack",
            u64::MAX,
            ok_stream(second.clone(), 10),
        )
        .await
        .expect("second put");

        let back = get_artifact(&pool, &owner, "scope-pack")
            .await
            .expect("get")
            .expect("stored");
        assert_eq!(back.data, second);

        // No orphan rows from the replaced payload.
        let artifact_count: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(artifact_count, 1);
        let chunk_count: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact_chunks")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(chunk_count, 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn oversize_put_rolls_back_and_stores_nothing(pool: sqlx::PgPool) {
        let owner = pod_owner();
        let data = payload(1024);
        let err = put_artifact(&pool, &owner, "scope-pack", 1023, ok_stream(data, 100))
            .await
            .expect_err("over the cap");
        match err {
            PutArtifactError::TooLarge { limit } => assert_eq!(limit, 1023),
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(
            get_artifact(&pool, &owner, "scope-pack")
                .await
                .expect("get")
                .is_none()
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 0, "the aborted transaction left no row");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn stream_error_rolls_back_and_stores_nothing(pool: sqlx::PgPool) {
        let owner = pod_owner();
        let pieces: Vec<Result<Vec<u8>, String>> =
            vec![Ok(payload(10)), Err("connection reset".to_string())];
        let err = put_artifact(&pool, &owner, "scope-pack", u64::MAX, stream::iter(pieces))
            .await
            .expect_err("stream errored");
        match err {
            PutArtifactError::Source(msg) => assert!(msg.contains("connection reset")),
            other => panic!("expected Source, got {other:?}"),
        }
        assert!(
            get_artifact(&pool, &owner, "scope-pack")
                .await
                .expect("get")
                .is_none()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn get_verifies_the_digest_against_the_chunks(pool: sqlx::PgPool) {
        let owner = pod_owner();
        let data = payload(64);
        let stored = put_artifact(&pool, &owner, "scope-pack", u64::MAX, ok_stream(data, 16))
            .await
            .expect("put");
        // Corrupt one chunk behind the store's back; the read must refuse the payload.
        sqlx::query("UPDATE artifact_chunks SET data = $1 WHERE artifact_id = $2 AND seq = 0")
            .bind(vec![0xAAu8; 64])
            .bind(stored.id)
            .execute(&pool)
            .await
            .expect("corrupt");
        // Same length, different bytes → digest mismatch.
        let err = get_artifact(&pool, &owner, "scope-pack")
            .await
            .expect_err("digest mismatch");
        assert!(err.to_string().contains("digest mismatch"), "{err:#}");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn owners_do_not_collide_across_kinds(pool: sqlx::PgPool) {
        // The same id under two owner kinds, and two kinds under one owner, stay distinct.
        let pod = ArtifactOwner::PodEvidence {
            pod: "same-id".to_string(),
        };
        let run = ArtifactOwner::Run {
            run_id: "same-id".to_string(),
        };
        put_artifact(
            &pool,
            &pod,
            "scope-pack",
            u64::MAX,
            ok_stream(vec![1u8; 4], 4),
        )
        .await
        .expect("pod put");
        put_artifact(
            &pool,
            &run,
            "scope-pack",
            u64::MAX,
            ok_stream(vec![2u8; 4], 4),
        )
        .await
        .expect("run put");
        put_artifact(
            &pool,
            &pod,
            "otel-log",
            u64::MAX,
            ok_stream(vec![3u8; 4], 4),
        )
        .await
        .expect("second kind put");

        let a = get_artifact(&pool, &pod, "scope-pack")
            .await
            .expect("get")
            .expect("row");
        let b = get_artifact(&pool, &run, "scope-pack")
            .await
            .expect("get")
            .expect("row");
        let c = get_artifact(&pool, &pod, "otel-log")
            .await
            .expect("get")
            .expect("row");
        assert_eq!(a.data, vec![1u8; 4]);
        assert_eq!(b.data, vec![2u8; 4]);
        assert_eq!(c.data, vec![3u8; 4]);
    }

    #[test]
    fn session_uri_round_trips_through_the_parser() {
        let uri = run_session_uri("run-2026-08-19-abc");
        assert_eq!(uri, "db://run-session/run-2026-08-19-abc/session.jsonl");
        assert_eq!(
            run_id_of_session_uri(&uri),
            Some("run-2026-08-19-abc"),
            "full uri parses"
        );
        assert_eq!(
            run_id_of_session_uri("db://run-session/run-x/"),
            Some("run-x"),
            "a prefix_of()-trimmed prefix parses too"
        );
        assert_eq!(run_id_of_session_uri("s3://b/run/session.jsonl"), None);
        assert_eq!(run_id_of_session_uri("/state/runs/x/session.jsonl"), None);
        assert_eq!(run_id_of_session_uri("db://run-session/"), None);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn run_session_round_trips_gzipped_and_replaces(pool: sqlx::PgPool) {
        assert!(
            get_run_session(&pool, "run-s")
                .await
                .expect("get")
                .is_none()
        );
        let ndjson = "{\"v\":1,\"kind\":\"shutdown\",\"outcome\":\"finished\"}\n";
        let stored = put_run_session(&pool, "run-s", ndjson.as_bytes())
            .await
            .expect("put");
        // Stored gzipped: the recorded size is the compressed payload's, not the text's.
        let owner = ArtifactOwner::Run {
            run_id: "run-s".to_string(),
        };
        let raw = get_artifact(
            &pool,
            &owner,
            crucible_contract::ArtifactKind::RunSession.as_str(),
        )
        .await
        .expect("get")
        .expect("row");
        assert!(raw.data.starts_with(&[0x1f, 0x8b]), "gzip at rest");
        assert_eq!(stored.bytes, raw.data.len() as u64);

        assert_eq!(
            get_run_session(&pool, "run-s").await.expect("get"),
            Some(ndjson.to_string())
        );

        // A re-put replaces in place.
        put_run_session(&pool, "run-s", b"second\n")
            .await
            .expect("replace");
        assert_eq!(
            get_run_session(&pool, "run-s").await.expect("get"),
            Some("second\n".to_string())
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn pack_tarball_roundtrips_and_replaces(pool: sqlx::PgPool) {
        assert!(
            get_pack_tarball(&pool, "owner_repo_7")
                .await
                .expect("get")
                .is_none()
        );
        let first = payload(2048);
        let digest = put_pack_tarball(&pool, "owner_repo_7", &first)
            .await
            .expect("put");
        assert_eq!(digest, content_digest(&first));
        assert_eq!(
            get_pack_tarball(&pool, "owner_repo_7").await.expect("get"),
            Some(first)
        );

        let second = payload(10);
        put_pack_tarball(&pool, "owner_repo_7", &second)
            .await
            .expect("replace");
        assert_eq!(
            get_pack_tarball(&pool, "owner_repo_7").await.expect("get"),
            Some(second)
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn steering_appends_in_order_per_issue(pool: sqlx::PgPool) {
        assert!(
            list_steering(&pool, "owner_repo_7")
                .await
                .expect("list")
                .is_empty()
        );
        let s1 = append_steering(&pool, "owner_repo_7", "try the cache", Some("wren"))
            .await
            .expect("append");
        let s2 = append_steering(&pool, "owner_repo_7", "narrower repro", None)
            .await
            .expect("append");
        // A different issue starts its own sequence.
        let other = append_steering(&pool, "owner_repo_8", "unrelated", None)
            .await
            .expect("append");
        assert_eq!((s1, s2, other), (1, 2, 1));

        let entries = list_steering(&pool, "owner_repo_7").await.expect("list");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[0].body_md, "try the cache");
        assert_eq!(entries[0].author.as_deref(), Some("wren"));
        assert_eq!(entries[1].seq, 2);
        assert!(entries[1].author.is_none());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn autopilot_is_a_single_upserted_row(pool: sqlx::PgPool) {
        assert!(get_autopilot(&pool).await.expect("get").is_none());
        let set = set_autopilot(&pool, false, Some("wren"), Some("cost runaway"))
            .await
            .expect("set");
        assert!(!set.enabled);

        let read = get_autopilot(&pool).await.expect("get").expect("row");
        assert_eq!(read, set);

        set_autopilot(&pool, true, None, None).await.expect("flip");
        let read = get_autopilot(&pool).await.expect("get").expect("row");
        assert!(read.enabled);
        assert!(read.changed_by.is_none());

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM autopilot")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 1);
    }
}

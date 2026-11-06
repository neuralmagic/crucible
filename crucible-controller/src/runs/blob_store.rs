//! Payload storage (migration `0004_shared_state.sql`): artifact bodies, pack tarballs, steering
//! appends, and the autopilot flag. This module owns the bytes; the existing `pod_artifacts`
//! pointer rows stay where they are.
//!
//! An artifact's `artifacts` row is always in Postgres. Its bytes are ordered gzipped chunks in
//! `artifact_chunks`, or, when the [`BlobStore`] has an [`ObjectStore`], one object named by the
//! row's `object_key`. Reads follow the row, so rows written before a bucket was configured stay
//! readable from Postgres.
//!
//! [`put_artifact`] is streaming-friendly: it consumes the body piece by piece, hashes inline,
//! and enforces the caller's cap mid-stream, so a torn or oversize upload leaves no row behind.
//! [`get_artifact`] verifies the size and digest before handing the payload back.

#![allow(clippy::disallowed_macros)]

use crate::playbooks::pack_trees::{PACK_COLS, PackRef};
use anyhow::{Context, Result, ensure};
use crucible_contract::content_digest;
use crucible_contract::pack_tree::{PackTree, TreeDigest};
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::fmt::Write as _;

pub mod objects;

use objects::ObjectStore;

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

/// Where artifact payloads go: the ledger pool, plus the bucket when one is configured.
#[derive(Clone, Debug)]
pub struct BlobStore {
    pool: PgPool,
    objects: Option<ObjectStore>,
}

impl BlobStore {
    /// Payload bytes in `artifact_chunks`.
    pub fn new(pool: PgPool) -> Self {
        BlobStore {
            pool,
            objects: None,
        }
    }

    /// New payload bytes go to `objects`; rows already in `artifact_chunks` stay readable.
    pub fn with_objects(mut self, objects: ObjectStore) -> Self {
        self.objects = Some(objects);
        self
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Whose payload an `artifacts` row holds. The variants mirror the `owner_kind` CHECK constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactOwner {
    /// A scope turn's upload; `pod` is the uploading pod's name.
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
/// same `(owner, kind)`. An error (including the cap) leaves the prior payload in place.
pub async fn put_artifact<S, B, E>(
    blobs: &BlobStore,
    owner: &ArtifactOwner,
    kind: &str,
    max_bytes: u64,
    stream: S,
) -> Result<StoredArtifact, PutArtifactError>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    match &blobs.objects {
        None => put_chunked(&blobs.pool, owner, kind, max_bytes, stream).await,
        Some(objects) => {
            Box::pin(put_object(
                &blobs.pool,
                objects,
                owner,
                kind,
                max_bytes,
                stream,
            ))
            .await
        }
    }
}

async fn put_chunked<S, B, E>(
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
    let prior_objects = take_prior_objects(&mut tx, owner, kind).await?;
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
    for key in prior_objects {
        tracing::warn!(
            key,
            "no evidence bucket is configured to delete a replaced artifact object"
        );
    }
    Ok(StoredArtifact { id, digest, bytes })
}

/// The bytes go to a fresh object first, then one transaction swaps the row to name it. The
/// replaced object is deleted only after that commit, so a reader never sees a row whose object
/// is gone.
async fn put_object<S, B, E>(
    pool: &PgPool,
    objects: &ObjectStore,
    owner: &ArtifactOwner,
    kind: &str,
    max_bytes: u64,
    stream: S,
) -> Result<StoredArtifact, PutArtifactError>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let key = objects.new_key();
    let uploaded = objects.upload(&key, max_bytes, stream).await?;
    let recorded = record_object(pool, owner, kind, &key, &uploaded).await;
    let (id, prior) = match recorded {
        Ok(recorded) => recorded,
        Err(e) => {
            delete_unreferenced(objects, &key).await;
            return Err(e.into());
        }
    };
    for prior in prior {
        delete_unreferenced(objects, &prior).await;
    }
    Ok(StoredArtifact {
        id,
        digest: uploaded.digest,
        bytes: uploaded.bytes,
    })
}

async fn record_object(
    pool: &PgPool,
    owner: &ArtifactOwner,
    kind: &str,
    key: &str,
    uploaded: &objects::Uploaded,
) -> Result<(i64, Vec<String>)> {
    let mut tx = pool.begin().await.context("begin put_artifact")?;
    let prior = take_prior_objects(&mut tx, owner, kind).await?;
    let id: i64 = sqlx::query_scalar(
        r#"INSERT INTO artifacts (owner_kind, owner_id, kind, digest, bytes, created_at, object_key)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id"#,
    )
    .bind(owner.owner_kind())
    .bind(owner.owner_id())
    .bind(kind)
    .bind(&uploaded.digest)
    .bind(i64::try_from(uploaded.bytes).context("artifact byte count exceeds i64")?)
    .bind(crate::clock::now_rfc3339())
    .bind(key)
    .fetch_one(&mut *tx)
    .await
    .context("inserting artifact row")?;
    tx.commit().await.context("commit put_artifact")?;
    Ok((id, prior))
}

/// Delete the `(owner, kind)` row, returning the object keys it named.
async fn take_prior_objects(
    tx: &mut Transaction<'_, Postgres>,
    owner: &ArtifactOwner,
    kind: &str,
) -> Result<Vec<String>> {
    let keys: Vec<Option<String>> = sqlx::query_scalar(
        r#"DELETE FROM artifacts WHERE owner_kind = $1 AND owner_id = $2 AND kind = $3
           RETURNING object_key"#,
    )
    .bind(owner.owner_kind())
    .bind(owner.owner_id())
    .bind(kind)
    .fetch_all(&mut **tx)
    .await
    .context("clearing prior artifact")?;
    Ok(keys.into_iter().flatten().collect())
}

async fn delete_unreferenced(objects: &ObjectStore, key: &str) {
    if let Err(e) = objects.delete(key).await {
        tracing::warn!(key, error = %format!("{e:#}"), "deleting an unreferenced artifact object");
    }
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
/// Errs if the payload disagrees with the recorded size or digest, or if the row names an object
/// and no bucket is configured.
pub async fn get_artifact(
    blobs: &BlobStore,
    owner: &ArtifactOwner,
    kind: &str,
) -> Result<Option<ArtifactPayload>> {
    let row = sqlx::query(
        r#"SELECT id, digest, bytes, object_key FROM artifacts
           WHERE owner_kind = $1 AND owner_id = $2 AND kind = $3"#,
    )
    .bind(owner.owner_kind())
    .bind(owner.owner_id())
    .bind(kind)
    .fetch_optional(&blobs.pool)
    .await
    .context("looking up artifact")?;
    let Some(row) = row else { return Ok(None) };
    let id: i64 = row.get("id");
    let digest: String = row.get("digest");
    let bytes: i64 = row.get("bytes");
    let object_key: Option<String> = row.get("object_key");

    let data = match (object_key, &blobs.objects) {
        (Some(key), Some(objects)) => Box::pin(objects.get(&key)).await?,
        (Some(key), None) => anyhow::bail!(
            "artifact {}/{}/{kind} is in object storage at {key}, and no evidence bucket is configured",
            owner.owner_kind(),
            owner.owner_id(),
        ),
        (None, _) => read_chunks(&blobs.pool, id, bytes).await?,
    };
    ensure!(
        i64::try_from(data.len()).ok() == Some(bytes),
        "artifact {}/{}/{kind}: {} payload bytes but the row records {bytes}",
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

async fn read_chunks(pool: &PgPool, id: i64, bytes: i64) -> Result<Vec<u8>> {
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
    Ok(data)
}

/// [`put_artifact`] over an in-memory payload.
pub async fn put_artifact_bytes(
    blobs: &BlobStore,
    owner: &ArtifactOwner,
    kind: &str,
    max_bytes: u64,
    data: Vec<u8>,
) -> Result<StoredArtifact, PutArtifactError> {
    let stream = futures_util::stream::iter([Ok::<Vec<u8>, std::convert::Infallible>(data)]);
    put_artifact(blobs, owner, kind, max_bytes, stream).await
}

/// Store a run's session NDJSON (gzipped at rest, like every ingested artifact). Replaces any
/// prior session for the run.
pub async fn put_run_session(
    blobs: &BlobStore,
    run_id: &str,
    ndjson: &[u8],
) -> Result<StoredArtifact> {
    use std::io::Write as _;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(ndjson).context("gzip run session")?;
    let gz = enc.finish().context("gzip run session")?;
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    let cap = crucible_contract::ArtifactKind::RunSession.max_bytes();
    put_artifact_bytes(
        blobs,
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
pub async fn put_run_engine_log(
    blobs: &BlobStore,
    run_id: &str,
    text: &str,
) -> Result<StoredArtifact> {
    use std::io::Write as _;
    let kept = engine_log_tail(text);
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(kept.as_bytes())
        .context("gzip run engine log")?;
    let gz = enc.finish().context("gzip run engine log")?;
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    put_artifact_bytes(blobs, &owner, RUN_ENGINE_LOG_KIND, u64::MAX, gz)
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
pub async fn get_run_engine_log(blobs: &BlobStore, run_id: &str) -> Result<Option<String>> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    let Some(payload) = get_artifact(blobs, &owner, RUN_ENGINE_LOG_KIND).await? else {
        return Ok(None);
    };
    Ok(Some(gunzip_maybe(&payload.data)?))
}

/// Store one of a run's tarred state directories, its captured files or its git memory: the
/// gzipped tar the loop POSTed, kept exactly as ingested. Replaces any prior bundle of that kind.
pub async fn put_run_bundle(
    blobs: &BlobStore,
    run_id: &str,
    kind: crucible_contract::ArtifactKind,
    tar_gz: Vec<u8>,
) -> Result<StoredArtifact> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    put_artifact_bytes(blobs, &owner, kind.as_str(), kind.max_bytes(), tar_gz)
        .await
        .map_err(anyhow::Error::from)
}

/// Read a run's bundle of `kind` back as the gzipped tar bytes, or `None` if never stored.
pub async fn get_run_bundle(
    blobs: &BlobStore,
    run_id: &str,
    kind: crucible_contract::ArtifactKind,
) -> Result<Option<Vec<u8>>> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    Ok(get_artifact(blobs, &owner, kind.as_str())
        .await?
        .map(|payload| payload.data))
}

/// Read a run's stored session back as NDJSON text, or `None` if never stored.
pub async fn get_run_session(blobs: &BlobStore, run_id: &str) -> Result<Option<String>> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    let Some(payload) = get_artifact(
        blobs,
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

/// What a pack write stored: the content digest of the tarball written to the legacy bytes
/// column, and the tree it encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPack {
    pub digest: String,
    pub tree: TreeDigest,
}

/// Store (or replace) one sanitized issue key's frozen pack: the tree, and its tarball beside it.
pub(crate) async fn put_pack(
    conn: &mut sqlx::PgConnection,
    issue_slug: &str,
    pack: &crate::playbooks::pack_trees::EncodedPack,
) -> Result<StoredPack> {
    let tarball = pack.tarball();
    let digest = pack.tarball_digest().to_string();
    let mut tx = sqlx::Connection::begin(&mut *conn)
        .await
        .context("opening the pack store transaction")?;
    let tree_digest = crate::playbooks::pack_trees::put_tree(&mut tx, pack).await?;
    sqlx::query(
        r#"INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at, tree_digest)
           VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (issue_slug) DO UPDATE SET
               tar_gz = excluded.tar_gz, digest = excluded.digest, bytes = excluded.bytes,
               created_at = excluded.created_at, tree_digest = excluded.tree_digest"#,
    )
    .bind(issue_slug)
    .bind(tarball)
    .bind(&digest)
    .bind(i64::try_from(tarball.len()).context("pack size")?)
    .bind(crate::clock::now_rfc3339())
    .bind(tree_digest.as_str())
    .execute(&mut *tx)
    .await
    .context("storing pack tarball")?;
    tx.commit().await.context("committing the pack store")?;
    Ok(StoredPack {
        digest,
        tree: tree_digest,
    })
}

/// Where one stored pack is, or `None` if never stored. A row with no tree yet names its legacy
/// bytes, digest-verified first.
pub(crate) async fn pack_ref(pool: &PgPool, issue_slug: &str) -> Result<Option<PackRef>> {
    let row = sqlx::query(const_format::formatcp!(
        "SELECT {PACK_COLS}, digest FROM pack_tarballs WHERE issue_slug = $1"
    ))
    .bind(issue_slug)
    .fetch_optional(pool)
    .await
    .context("reading a stored pack")?;
    let Some(row) = row else { return Ok(None) };
    let pack =
        PackRef::from_row(&row)?.with_context(|| format!("pack {issue_slug} holds no pack"))?;
    if let PackRef::Legacy(bytes) = &pack {
        let digest: String = row.try_get("digest")?;
        let got = content_digest(bytes);
        ensure!(
            got == digest,
            "pack {issue_slug}: digest mismatch, stored {digest}, read {got}"
        );
    }
    Ok(Some(pack))
}

/// Read one stored pack back as its tree, or `None` if never stored.
pub async fn get_pack(pool: &PgPool, issue_slug: &str) -> Result<Option<PackTree>> {
    let Some(pack) = pack_ref(pool, issue_slug).await? else {
        return Ok(None);
    };
    crate::playbooks::pack_trees::load(pool, pack)
        .await
        .with_context(|| format!("reading pack {issue_slug}"))
        .map(Some)
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
    use crate::runs::blob_store::objects::BucketUri;
    use crate::runs::blob_store::*;
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region, SharedCredentialsProvider};
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

    async fn artifact_rows(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM artifacts")
            .fetch_one(pool)
            .await
            .expect("count artifacts")
    }

    async fn chunk_rows(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM artifact_chunks")
            .fetch_one(pool)
            .await
            .expect("count chunks")
    }

    /// A payload over one Postgres chunk and one multipart part, streamed in awkward pieces.
    async fn roundtrips_a_multi_chunk_payload(blobs: &BlobStore) -> StoredArtifact {
        let data = payload(2 * ARTIFACT_CHUNK_BYTES + 12345);
        let owner = pod_owner();
        let stored = put_artifact(
            blobs,
            &owner,
            "run-session",
            data.len() as u64,
            ok_stream(data.clone(), 1_000_003),
        )
        .await
        .expect("put");
        assert_eq!(stored.bytes, data.len() as u64);
        assert_eq!(stored.digest, content_digest(&data));
        let back = get_artifact(blobs, &owner, "run-session")
            .await
            .expect("get")
            .expect("stored");
        assert_eq!(back.data, data);
        assert_eq!(back.digest, stored.digest);
        stored
    }

    async fn replaces_a_prior_payload(blobs: &BlobStore) {
        let owner = pod_owner();
        put_artifact(
            blobs,
            &owner,
            "scope-pack",
            u64::MAX,
            ok_stream(payload(ARTIFACT_CHUNK_BYTES + 7), 4096),
        )
        .await
        .expect("first put");
        let second = payload(99);
        put_artifact(
            blobs,
            &owner,
            "scope-pack",
            u64::MAX,
            ok_stream(second.clone(), 10),
        )
        .await
        .expect("second put");
        let back = get_artifact(blobs, &owner, "scope-pack")
            .await
            .expect("get")
            .expect("stored");
        assert_eq!(back.data, second);
        assert_eq!(artifact_rows(blobs.pool()).await, 1);
    }

    /// Ten MiB against a nine MiB cap: one multipart part is already up when the cap trips.
    async fn refuses_an_oversize_put_and_keeps_the_prior_payload(blobs: &BlobStore) {
        let owner = pod_owner();
        put_artifact_bytes(blobs, &owner, "scope-pack", u64::MAX, b"prior".to_vec())
            .await
            .expect("prior put");
        let cap = 9 * 1024 * 1024;
        let err = put_artifact(
            blobs,
            &owner,
            "scope-pack",
            cap,
            ok_stream(payload(10 * 1024 * 1024), 1024 * 1024),
        )
        .await
        .expect_err("over the cap");
        match err {
            PutArtifactError::TooLarge { limit } => assert_eq!(limit, cap),
            other => panic!("expected TooLarge, got {other:?}"),
        }
        let back = get_artifact(blobs, &owner, "scope-pack")
            .await
            .expect("get")
            .expect("prior kept");
        assert_eq!(back.data, b"prior");
        assert_eq!(artifact_rows(blobs.pool()).await, 1);
    }

    async fn refuses_a_failed_stream_and_stores_nothing(blobs: &BlobStore) {
        let owner = pod_owner();
        let pieces: Vec<Result<Vec<u8>, String>> = vec![
            Ok(payload(9 * 1024 * 1024)),
            Err("connection reset".to_string()),
        ];
        let err = put_artifact(blobs, &owner, "scope-pack", u64::MAX, stream::iter(pieces))
            .await
            .expect_err("stream errored");
        match err {
            PutArtifactError::Source(msg) => assert!(msg.contains("connection reset")),
            other => panic!("expected Source, got {other:?}"),
        }
        assert!(
            get_artifact(blobs, &owner, "scope-pack")
                .await
                .expect("get")
                .is_none()
        );
        assert_eq!(artifact_rows(blobs.pool()).await, 0);
    }

    async fn keeps_owners_and_kinds_apart(blobs: &BlobStore) {
        let pod = ArtifactOwner::PodEvidence {
            pod: "same-id".to_string(),
        };
        let run = ArtifactOwner::Run {
            run_id: "same-id".to_string(),
        };
        for (owner, kind, byte) in [
            (&pod, "scope-pack", 1u8),
            (&run, "scope-pack", 2),
            (&pod, "otel-log", 3),
        ] {
            put_artifact(blobs, owner, kind, u64::MAX, ok_stream(vec![byte; 4], 4))
                .await
                .expect("put");
        }
        for (owner, kind, byte) in [
            (&pod, "scope-pack", 1u8),
            (&run, "scope-pack", 2),
            (&pod, "otel-log", 3),
        ] {
            let got = get_artifact(blobs, owner, kind)
                .await
                .expect("get")
                .expect("row");
            assert_eq!(got.data, vec![byte; 4], "{owner:?} {kind}");
        }
    }

    async fn roundtrips_an_empty_payload(blobs: &BlobStore) {
        let owner = pod_owner();
        let stored = put_artifact_bytes(blobs, &owner, "otel-log", u64::MAX, Vec::new())
            .await
            .expect("put");
        assert_eq!(stored.bytes, 0);
        let back = get_artifact(blobs, &owner, "otel-log")
            .await
            .expect("get")
            .expect("row");
        assert!(back.data.is_empty());
        assert_eq!(back.digest, content_digest(b""));
    }

    async fn roundtrips_a_gzipped_run_session(blobs: &BlobStore) {
        assert!(
            get_run_session(blobs, "run-s")
                .await
                .expect("get")
                .is_none()
        );
        let ndjson = "{\"v\":1,\"kind\":\"shutdown\",\"outcome\":\"finished\"}\n";
        let stored = put_run_session(blobs, "run-s", ndjson.as_bytes())
            .await
            .expect("put");
        let owner = ArtifactOwner::Run {
            run_id: "run-s".to_string(),
        };
        let raw = get_artifact(
            blobs,
            &owner,
            crucible_contract::ArtifactKind::RunSession.as_str(),
        )
        .await
        .expect("get")
        .expect("row");
        assert!(raw.data.starts_with(&[0x1f, 0x8b]), "gzip at rest");
        assert_eq!(stored.bytes, raw.data.len() as u64);
        assert_eq!(
            get_run_session(blobs, "run-s").await.expect("get"),
            Some(ndjson.to_string())
        );
        put_run_session(blobs, "run-s", b"second\n")
            .await
            .expect("replace");
        assert_eq!(
            get_run_session(blobs, "run-s").await.expect("get"),
            Some("second\n".to_string())
        );
    }

    async fn roundtrips_a_run_engine_log_and_bundle(blobs: &BlobStore) {
        assert_eq!(get_run_engine_log(blobs, "run-x").await.unwrap(), None);
        put_run_engine_log(blobs, "run-x", "first\n").await.unwrap();
        put_run_engine_log(blobs, "run-x", "gateway_boot: ok\nagent: exit 0\n")
            .await
            .unwrap();
        assert_eq!(
            get_run_engine_log(blobs, "run-x").await.unwrap().as_deref(),
            Some("gateway_boot: ok\nagent: exit 0\n")
        );
        let kind = crucible_contract::ArtifactKind::RunFiles;
        assert_eq!(get_run_bundle(blobs, "run-x", kind).await.unwrap(), None);
        put_run_bundle(blobs, "run-x", kind, payload(4096))
            .await
            .unwrap();
        assert_eq!(
            get_run_bundle(blobs, "run-x", kind).await.unwrap(),
            Some(payload(4096))
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn postgres_roundtrips_a_multi_chunk_payload(pool: PgPool) {
        let blobs = BlobStore::new(pool);
        let stored = roundtrips_a_multi_chunk_payload(&blobs).await;
        let chunks: i64 =
            sqlx::query_scalar("SELECT count(*) FROM artifact_chunks WHERE artifact_id = $1")
                .bind(stored.id)
                .fetch_one(blobs.pool())
                .await
                .expect("count");
        assert_eq!(chunks, 3);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn postgres_replaces_a_prior_payload_without_orphan_chunks(pool: PgPool) {
        let blobs = BlobStore::new(pool);
        replaces_a_prior_payload(&blobs).await;
        assert_eq!(chunk_rows(blobs.pool()).await, 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn postgres_refuses_an_oversize_put_and_keeps_the_prior_payload(pool: PgPool) {
        let blobs = BlobStore::new(pool);
        refuses_an_oversize_put_and_keeps_the_prior_payload(&blobs).await;
        assert_eq!(chunk_rows(blobs.pool()).await, 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn postgres_refuses_a_failed_stream_and_stores_nothing(pool: PgPool) {
        let blobs = BlobStore::new(pool);
        refuses_a_failed_stream_and_stores_nothing(&blobs).await;
        assert_eq!(chunk_rows(blobs.pool()).await, 0);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn postgres_keeps_owners_and_kinds_apart(pool: PgPool) {
        keeps_owners_and_kinds_apart(&BlobStore::new(pool)).await;
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn postgres_roundtrips_an_empty_payload(pool: PgPool) {
        roundtrips_an_empty_payload(&BlobStore::new(pool)).await;
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn postgres_roundtrips_a_gzipped_run_session(pool: PgPool) {
        roundtrips_a_gzipped_run_session(&BlobStore::new(pool)).await;
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn postgres_roundtrips_a_run_engine_log_and_bundle(pool: PgPool) {
        roundtrips_a_run_engine_log_and_bundle(&BlobStore::new(pool)).await;
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn get_verifies_the_digest_against_the_chunks(pool: PgPool) {
        let blobs = BlobStore::new(pool);
        let owner = pod_owner();
        let stored = put_artifact(
            &blobs,
            &owner,
            "scope-pack",
            u64::MAX,
            ok_stream(payload(64), 16),
        )
        .await
        .expect("put");
        // Same length, different bytes: only the digest can catch it.
        sqlx::query("UPDATE artifact_chunks SET data = $1 WHERE artifact_id = $2 AND seq = 0")
            .bind(vec![0xAAu8; 64])
            .bind(stored.id)
            .execute(blobs.pool())
            .await
            .expect("corrupt");
        let err = get_artifact(&blobs, &owner, "scope-pack")
            .await
            .expect_err("digest mismatch");
        assert!(err.to_string().contains("digest mismatch"), "{err:#}");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_row_in_object_storage_is_refused_without_a_bucket(pool: PgPool) {
        sqlx::query(
            r#"INSERT INTO artifacts (owner_kind, owner_id, kind, digest, bytes, created_at, object_key)
               VALUES ('run-session', 'run-o', 'run-files', 'sha256:00', 3, '2026-11-05T00:00:00Z', 'artifacts/k')"#,
        )
        .execute(&pool)
        .await
        .expect("seed");
        let err = get_run_bundle(
            &BlobStore::new(pool),
            "run-o",
            crucible_contract::ArtifactKind::RunFiles,
        )
        .await
        .expect_err("no bucket");
        assert!(
            err.to_string().contains("no evidence bucket is configured"),
            "{err:#}"
        );
    }

    /// A per-test key prefix in a shared bucket on the S3-compatible server at
    /// `CRUCIBLE_TEST_S3_ENDPOINT`, and a raw client to inspect it with.
    struct S3Env {
        blobs: BlobStore,
        raw: aws_sdk_s3::Client,
        root: String,
    }

    const S3_TEST_BUCKET: &str = "crucible-test-evidence";

    fn s3_required() -> bool {
        std::env::var("CRUCIBLE_REQUIRE_S3_TESTS")
            .map(|v| !v.trim().is_empty() && v != "0")
            .unwrap_or(false)
    }

    fn s3_sdk_config() -> Option<aws_config::SdkConfig> {
        let Some(endpoint) = std::env::var("CRUCIBLE_TEST_S3_ENDPOINT")
            .ok()
            .filter(|v| !v.is_empty())
        else {
            assert!(
                !s3_required(),
                "CRUCIBLE_REQUIRE_S3_TESTS is set but CRUCIBLE_TEST_S3_ENDPOINT is not"
            );
            eprintln!("skipping: CRUCIBLE_TEST_S3_ENDPOINT is unset (see `just dev-s3`)");
            return None;
        };
        let key = |var: &str| std::env::var(var).unwrap_or_else(|_| "crucible-test".to_string());
        let creds = Credentials::new(
            key("CRUCIBLE_TEST_S3_ACCESS_KEY"),
            key("CRUCIBLE_TEST_S3_SECRET_KEY"),
            None,
            None,
            "crucible-test",
        );
        Some(
            aws_config::SdkConfig::builder()
                .endpoint_url(endpoint)
                .region(Region::new("us-east-1"))
                .credentials_provider(SharedCredentialsProvider::new(creds))
                .behavior_version(BehaviorVersion::latest())
                .build(),
        )
    }

    impl S3Env {
        async fn new(pool: PgPool) -> Option<S3Env> {
            let conf = s3_sdk_config()?;
            let raw = aws_sdk_s3::Client::from_conf(
                aws_sdk_s3::config::Builder::from(&conf)
                    .force_path_style(true)
                    .build(),
            );
            if let Err(create) = raw.create_bucket().bucket(S3_TEST_BUCKET).send().await {
                raw.head_bucket()
                    .bucket(S3_TEST_BUCKET)
                    .send()
                    .await
                    .unwrap_or_else(|head| panic!("create bucket: {create:?}; head: {head:?}"));
            }
            let root = format!("t-{}", uuid::Uuid::now_v7().simple());
            let location: BucketUri = format!("s3://{S3_TEST_BUCKET}/{root}/")
                .parse()
                .expect("uri");
            let objects = ObjectStore::from_sdk_config(&conf, location);
            objects.check().await.expect("bucket reachable");
            Some(S3Env {
                blobs: BlobStore::new(pool).with_objects(objects),
                raw,
                root,
            })
        }

        async fn keys(&self) -> Vec<String> {
            let out = self
                .raw
                .list_objects_v2()
                .bucket(S3_TEST_BUCKET)
                .prefix(format!("{}/", self.root))
                .send()
                .await
                .expect("list objects");
            out.contents()
                .iter()
                .filter_map(|o| o.key().map(str::to_string))
                .collect()
        }

        async fn uploads_in_progress(&self) -> usize {
            self.raw
                .list_multipart_uploads()
                .bucket(S3_TEST_BUCKET)
                .prefix(format!("{}/", self.root))
                .send()
                .await
                .expect("list multipart uploads")
                .uploads()
                .len()
        }

        async fn object_key_of(&self, id: i64) -> String {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT object_key FROM artifacts WHERE id = $1",
            )
            .bind(id)
            .fetch_one(self.blobs.pool())
            .await
            .expect("row")
            .expect("object key")
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_roundtrips_a_multi_part_payload_under_the_prefix(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        let stored = roundtrips_a_multi_chunk_payload(&env.blobs).await;
        let key = env.object_key_of(stored.id).await;
        assert!(
            key.starts_with(&format!("{}/artifacts/", env.root)),
            "{key}"
        );
        assert_eq!(env.keys().await, vec![key]);
        assert_eq!(
            chunk_rows(env.blobs.pool()).await,
            0,
            "no bytes in Postgres"
        );
        assert_eq!(env.uploads_in_progress().await, 0);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_replaces_a_prior_payload_and_deletes_its_object(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        replaces_a_prior_payload(&env.blobs).await;
        assert_eq!(env.keys().await.len(), 1);
        assert_eq!(chunk_rows(env.blobs.pool()).await, 0);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_refuses_an_oversize_put_and_aborts_its_upload(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        refuses_an_oversize_put_and_keeps_the_prior_payload(&env.blobs).await;
        assert_eq!(env.keys().await.len(), 1, "only the prior payload's object");
        assert_eq!(env.uploads_in_progress().await, 0);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_refuses_a_failed_stream_and_aborts_its_upload(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        refuses_a_failed_stream_and_stores_nothing(&env.blobs).await;
        assert!(env.keys().await.is_empty());
        assert_eq!(env.uploads_in_progress().await, 0);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_keeps_owners_and_kinds_apart(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        keeps_owners_and_kinds_apart(&env.blobs).await;
        assert_eq!(env.keys().await.len(), 3);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_roundtrips_an_empty_payload(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        roundtrips_an_empty_payload(&env.blobs).await;
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_roundtrips_a_gzipped_run_session(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        roundtrips_a_gzipped_run_session(&env.blobs).await;
        assert_eq!(env.keys().await.len(), 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_roundtrips_a_run_engine_log_and_bundle(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        roundtrips_a_run_engine_log_and_bundle(&env.blobs).await;
        assert_eq!(env.keys().await.len(), 2);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_get_verifies_the_digest_against_the_object(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        let owner = pod_owner();
        let stored = put_artifact_bytes(&env.blobs, &owner, "scope-pack", u64::MAX, payload(64))
            .await
            .expect("put");
        env.raw
            .put_object()
            .bucket(S3_TEST_BUCKET)
            .key(env.object_key_of(stored.id).await)
            .body(vec![0xAAu8; 64].into())
            .send()
            .await
            .expect("overwrite behind the store");
        let err = get_artifact(&env.blobs, &owner, "scope-pack")
            .await
            .expect_err("digest mismatch");
        assert!(err.to_string().contains("digest mismatch"), "{err:#}");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_reads_a_row_written_to_postgres_and_moves_it_on_rewrite(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        let postgres = BlobStore::new(env.blobs.pool().clone());
        put_run_session(&postgres, "run-old", b"before the bucket\n")
            .await
            .expect("put to postgres");
        assert_eq!(
            get_run_session(&env.blobs, "run-old").await.expect("get"),
            Some("before the bucket\n".to_string())
        );
        assert!(env.keys().await.is_empty());

        put_run_session(&env.blobs, "run-old", b"after\n")
            .await
            .expect("rewrite");
        assert_eq!(chunk_rows(env.blobs.pool()).await, 0);
        assert_eq!(env.keys().await.len(), 1);
        assert_eq!(
            get_run_session(&env.blobs, "run-old").await.expect("get"),
            Some("after\n".to_string())
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn s3_check_refuses_a_missing_bucket(pool: PgPool) {
        let Some(env) = S3Env::new(pool).await else {
            return;
        };
        let conf = s3_sdk_config().expect("endpoint");
        let missing: BucketUri = format!("s3://{S3_TEST_BUCKET}-{}", env.root)
            .parse()
            .expect("uri");
        let err = ObjectStore::from_sdk_config(&conf, missing)
            .check()
            .await
            .expect_err("missing bucket");
        assert!(err.to_string().contains("is not reachable"), "{err:#}");
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
    async fn a_pack_roundtrips_and_replaces_with_its_tree(pool: sqlx::PgPool) {
        assert!(
            get_pack(&pool, "owner_repo_7")
                .await
                .expect("get")
                .is_none()
        );
        let mut conn = pool.acquire().await.expect("conn");
        let stored_tree = || async {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT tree_digest FROM pack_tarballs WHERE issue_slug = 'owner_repo_7'",
            )
            .fetch_one(&pool)
            .await
            .expect("tree column")
        };

        let first = PackTree::from_pairs(&[("crucible.toml", b"m"), ("big", &payload(2048))])
            .expect("tree");
        let encoded =
            crate::playbooks::pack_trees::EncodedPack::new(first.clone()).expect("encode");
        let tarball = encoded.tarball();
        let stored = put_pack(&mut conn, "owner_repo_7", &encoded)
            .await
            .expect("put");
        assert_eq!(
            stored,
            StoredPack {
                digest: content_digest(tarball),
                tree: first.digest(),
            }
        );
        assert_eq!(
            get_pack(&pool, "owner_repo_7").await.expect("get"),
            Some(first.clone())
        );
        assert_eq!(stored_tree().await, Some(first.digest().to_string()));

        let second = PackTree::from_pairs(&[("crucible.toml", b"n")]).expect("tree");
        put_pack(
            &mut conn,
            "owner_repo_7",
            &crate::playbooks::pack_trees::EncodedPack::new(second.clone()).expect("encode"),
        )
        .await
        .expect("replace");
        assert_eq!(
            get_pack(&pool, "owner_repo_7").await.expect("get"),
            Some(second.clone())
        );
        assert_eq!(stored_tree().await, Some(second.digest().to_string()));
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

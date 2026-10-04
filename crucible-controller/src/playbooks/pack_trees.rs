//! Pack trees stored once by `tree1:` digest (RFC-0002:C-PACK-BASE, ADR-0059): `pack_trees` holds
//! one row per tree and `pack_tree_files` its files. Until every legacy pack-byte column is
//! converted, [`load`] reads a row's tree when it has one and its legacy tarball when it does not,
//! so a row a standby wrote, or one conversion has not reached, still reads.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail};
use crucible_contract::pack_tree::{PackFilePath, PackTree, TreeDigest};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgExecutor, Row};
use std::collections::BTreeMap;

/// A pack tree paired with its own tarball, `tree.tarball()`: the bytes a legacy column holds
/// beside the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EncodedPack {
    tree: PackTree,
    tarball: Vec<u8>,
}

impl EncodedPack {
    pub(crate) fn new(tree: PackTree) -> std::io::Result<Self> {
        let tarball = tree.tarball()?;
        Ok(Self { tree, tarball })
    }

    pub(crate) fn tree(&self) -> &PackTree {
        &self.tree
    }

    pub(crate) fn tarball(&self) -> &[u8] {
        &self.tarball
    }
}

/// Store `pack`'s tree under its digest unless it is already stored, and return the digest.
pub(crate) async fn put_tree(conn: &mut PgConnection, pack: &EncodedPack) -> Result<TreeDigest> {
    let (tree, tarball) = (pack.tree(), pack.tarball());
    let (digest, file_hashes) = tree.digest_with_file_hashes();
    let stored: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM pack_trees WHERE digest = $1 FOR KEY SHARE")
            .bind(digest.as_str())
            .fetch_optional(&mut *conn)
            .await
            .context("looking up a pack tree")?;
    if stored.is_some() {
        return Ok(digest);
    }
    let total: usize = tree.files().values().map(Vec::len).sum();
    let inserted = sqlx::query(
        "INSERT INTO pack_trees
             (digest, file_count, total_bytes, delivered_bytes, tarball_digest, created_at)
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (digest) DO NOTHING",
    )
    .bind(digest.as_str())
    .bind(i32::try_from(tree.files().len()).context("pack file count")?)
    .bind(i64::try_from(total).context("pack size")?)
    .bind(i64::try_from(tarball.len()).context("pack delivered size")?)
    .bind(crucible_contract::content_digest(tarball))
    .bind(crate::clock::now_rfc3339())
    .execute(&mut *conn)
    .await
    .context("storing a pack tree")?
    .rows_affected();
    if inserted == 1 {
        let paths: Vec<&str> = tree.files().keys().map(|p| p.as_str()).collect();
        let contents: Vec<&[u8]> = tree.files().values().map(Vec::as_slice).collect();
        sqlx::query(
            "INSERT INTO pack_tree_files (digest, path, sha256, content)
             SELECT $1, * FROM UNNEST($2::TEXT[], $3::TEXT[], $4::BYTEA[])",
        )
        .bind(digest.as_str())
        .bind(&paths)
        .bind(&file_hashes)
        .bind(&contents)
        .execute(&mut *conn)
        .await
        .context("storing pack files")?;
    }
    Ok(digest)
}

/// Every `(table, column)` that pins a stored tree. Aliases and draft or binding provenance name
/// trees without keeping them.
const TREE_PINS: [(&str, &str); 7] = [
    ("playbooks", "tree_digest"),
    ("playbook_revisions", "tree_digest"),
    ("pack_imports", "tree_digest"),
    ("playbook_draft_versions", "tree_digest"),
    ("playbook_standing_launches", "adopted_tree_digest"),
    ("pack_tarballs", "tree_digest"),
    ("scopes", "tree_digest"),
];

/// How old an unpinned tree must be before [`collect`] deletes it.
const COLLECT_AFTER: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

/// Delete every tree older than [`COLLECT_AFTER`] that no [`TREE_PINS`] column names, and return
/// how many went. A row that pins one of them while this runs makes the foreign key refuse the
/// delete, which is an error with nothing deleted.
pub async fn collect(pool: &sqlx::PgPool) -> Result<u64> {
    let cutoff = jiff::Timestamp::now()
        .checked_sub(COLLECT_AFTER)
        .context("computing the pack tree cutoff")?;
    let unpinned: String = TREE_PINS
        .iter()
        .map(|(table, column)| {
            format!(" AND NOT EXISTS (SELECT 1 FROM {table} WHERE {column} = t.digest)")
        })
        .collect();
    let deleted = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM pack_trees t WHERE t.created_at < $1{unpinned}"
    )))
    .bind(crate::clock::stamp(cutoff))
    .execute(pool)
    .await
    .context("deleting unpinned pack trees")?
    .rows_affected();
    Ok(deleted)
}

/// The columns [`PackRef::from_row`] reads, for a table with `tree_digest` and `tar_gz` columns.
/// The legacy bytes are only fetched when the row has no tree.
pub(crate) const PACK_COLS: &str =
    "tree_digest, CASE WHEN tree_digest IS NULL THEN tar_gz END AS tar_gz";

/// Where a row's pack is: the stored tree its tree column names, or the legacy bytes of a row
/// whose tree column is NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PackRef {
    Tree(TreeDigest),
    Legacy(Vec<u8>),
}

impl PackRef {
    /// The pack a row names through its `tree_digest` and `tar_gz` columns, `None` when both are
    /// NULL. The tree wins when both are set.
    pub(crate) fn from_row(row: &PgRow) -> Result<Option<Self>> {
        let tree: Option<String> = row.try_get("tree_digest")?;
        if let Some(tree) = tree {
            return Ok(Some(Self::Tree(tree.parse().map_err(anyhow::Error::msg)?)));
        }
        let bytes: Option<Vec<u8>> = row.try_get("tar_gz")?;
        Ok(bytes.map(Self::Legacy))
    }
}

/// The tree stored under `digest`, or `None` when none is. A tree whose stored files no longer
/// hash to its digest is an error, never returned.
pub(crate) async fn get_tree(
    ex: impl PgExecutor<'_>,
    digest: &TreeDigest,
) -> Result<Option<PackTree>> {
    let rows = sqlx::query(
        "SELECT f.path, f.content FROM pack_trees t
         LEFT JOIN pack_tree_files f ON f.digest = t.digest
         WHERE t.digest = $1",
    )
    .bind(digest.as_str())
    .fetch_all(ex)
    .await
    .with_context(|| format!("reading pack tree {digest}"))?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut files = BTreeMap::new();
    for row in rows {
        let path: Option<String> = row.try_get("path")?;
        if let Some(path) = path {
            let path: PackFilePath = path
                .parse()
                .with_context(|| format!("pack tree {digest}"))?;
            files.insert(path, row.try_get::<Vec<u8>, _>("content")?);
        }
    }
    let tree = PackTree::new(files).with_context(|| format!("pack tree {digest}"))?;
    if &tree.digest() != digest {
        bail!("pack tree {digest} no longer matches its stored files");
    }
    Ok(Some(tree))
}

/// One file of the tree stored under `digest`, `None` when the tree holds no such path.
#[cfg(any(test, feature = "autoresearch"))]
pub(crate) async fn read_file(
    ex: impl PgExecutor<'_>,
    digest: &TreeDigest,
    path: &str,
) -> Result<Option<Vec<u8>>> {
    sqlx::query_scalar("SELECT content FROM pack_tree_files WHERE digest = $1 AND path = $2")
        .bind(digest.as_str())
        .bind(path)
        .fetch_optional(ex)
        .await
        .with_context(|| format!("reading {path} of pack tree {digest}"))
}

/// What a digest a caller supplied names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    Current(TreeDigest),
    Superseded { replacement: TreeDigest },
    Unconvertible { reason: String },
    Unknown,
}

/// Resolve a digest a caller supplied: a stored tree, a pre-tree digest and what it became, or
/// nothing this controller knows. The caller decides what each answer may disclose.
pub(crate) async fn resolve_supplied(ex: impl PgExecutor<'_>, supplied: &str) -> Result<Resolved> {
    if let Ok(digest) = supplied.parse::<TreeDigest>() {
        let stored: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pack_trees WHERE digest = $1")
            .bind(digest.as_str())
            .fetch_optional(ex)
            .await
            .context("resolving a pack digest")?;
        return Ok(match stored {
            Some(_) => Resolved::Current(digest),
            None => Resolved::Unknown,
        });
    }
    let alias = sqlx::query(
        "SELECT tree_digest, unconvertible_reason FROM pack_digest_aliases WHERE old_digest = $1",
    )
    .bind(supplied)
    .fetch_optional(ex)
    .await
    .context("resolving a pre-tree pack digest")?;
    let Some(alias) = alias else {
        return Ok(Resolved::Unknown);
    };
    let tree: Option<String> = alias.try_get("tree_digest")?;
    let reason: Option<String> = alias.try_get("unconvertible_reason")?;
    Ok(match (tree, reason) {
        (Some(tree), _) => Resolved::Superseded {
            replacement: tree.parse().map_err(anyhow::Error::msg)?,
        },
        (None, Some(reason)) => Resolved::Unconvertible { reason },
        (None, None) => Resolved::Unknown,
    })
}

/// The tree that superseded `supplied`, when `supplied` is a pre-tree digest whose tree playbook
/// `playbook` has held. `None` for anything else, which the caller answers as an unknown digest.
/// Only for a caller who may read `playbook`.
pub(crate) async fn superseded_in(
    pool: &sqlx::PgPool,
    playbook: &str,
    supplied: &str,
) -> Result<Option<TreeDigest>> {
    let Resolved::Superseded { replacement } = resolve_supplied(pool, supplied).await? else {
        return Ok(None);
    };
    let held: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM playbook_revisions WHERE playbook_id = $1 AND tree_digest = $2",
    )
    .bind(playbook)
    .bind(replacement.as_str())
    .fetch_optional(pool)
    .await
    .context("checking a playbook's revisions")?;
    Ok(held.map(|_| replacement))
}

/// Legacy pack bytes that cannot be a tree, refused wherever they would be compiled, delivered,
/// launched, or used as a base.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("pack {digest} is unconvertible: {reason}")]
pub(crate) struct Unconvertible {
    pub(crate) digest: String,
    pub(crate) reason: String,
}

/// The refusal for legacy bytes that conversion recorded as unconvertible, `None` for any others.
pub(crate) async fn unconvertible(
    ex: impl PgExecutor<'_>,
    tar_gz: &[u8],
) -> Result<Option<Unconvertible>> {
    let digest = crucible_contract::content_digest(tar_gz);
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT unconvertible_reason FROM pack_digest_aliases
         WHERE old_digest = $1 AND unconvertible_reason IS NOT NULL",
    )
    .bind(&digest)
    .fetch_optional(ex)
    .await
    .context("looking up an unconvertible pack")?;
    Ok(reason.map(|reason| Unconvertible { digest, reason }))
}

/// The pack `pack` names: its stored tree, or its legacy bytes read as a tree. Legacy bytes that
/// cannot be a tree are an [`Unconvertible`] error.
pub(crate) async fn load(ex: impl PgExecutor<'_>, pack: PackRef) -> Result<PackTree> {
    match pack {
        PackRef::Tree(digest) => get_tree(ex, &digest)
            .await?
            .with_context(|| format!("pack tree {digest} is not stored")),
        PackRef::Legacy(bytes) => {
            if let Some(refusal) = unconvertible(ex, &bytes).await? {
                return Err(refusal.into());
            }
            read_legacy(bytes).await
        }
    }
}

/// The pack a row selected with [`PACK_COLS`] names.
pub(crate) async fn load_row(ex: impl PgExecutor<'_>, row: &PgRow) -> Result<PackTree> {
    let pack = PackRef::from_row(row)?.context("the row holds no pack")?;
    load(ex, pack).await
}

/// Legacy pack bytes read as a tree, off the async runtime.
async fn read_legacy(bytes: Vec<u8>) -> Result<PackTree> {
    let read = tokio::task::spawn_blocking(move || {
        crucible_contract::pack_tree::read_tar_gz(&bytes).map_err(|e| Unconvertible {
            digest: crucible_contract::content_digest(&bytes),
            reason: e.to_string(),
        })
    })
    .await
    .context("joining the legacy pack read worker")??;
    Ok(read.tree)
}

#[cfg(test)]
mod tests {
    use crate::playbooks::pack_trees::*;
    use sqlx::PgPool;

    fn pack(files: &[(&str, &[u8])]) -> PackTree {
        PackTree::from_pairs(files).expect("tree")
    }

    fn encoded(tree: &PackTree) -> EncodedPack {
        EncodedPack::new(tree.clone()).expect("encode")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn storing_a_tree_twice_keeps_one_copy(pool: PgPool) {
        let tree = pack(&[("crucible.toml", b"m"), ("tools/run.sh", b"r")]);
        let mut conn = pool.acquire().await.expect("conn");

        let first = put_tree(&mut conn, &encoded(&tree)).await.expect("put");
        let second = put_tree(&mut conn, &encoded(&tree))
            .await
            .expect("put again");

        assert_eq!(first, second);
        assert_eq!(first, tree.digest());
        let counts: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM pack_trees), (SELECT count(*) FROM pack_tree_files)",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(counts, (1, 2));
        let stored: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT path, content FROM pack_tree_files WHERE digest = $1 ORDER BY path",
        )
        .bind(first.as_str())
        .fetch_all(&pool)
        .await
        .expect("files");
        assert_eq!(
            stored,
            vec![
                ("crucible.toml".to_string(), b"m".to_vec()),
                ("tools/run.sh".to_string(), b"r".to_vec())
            ]
        );
    }

    async fn seed_legacy_row(pool: &PgPool, tree: &str) {
        sqlx::query("DELETE FROM pack_tarballs")
            .execute(pool)
            .await
            .expect("clear");
        sqlx::query(
            "INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at, tree_digest)
             VALUES ('s', 'legacy', 'sha256:x', 1, 'now', $1)",
        )
        .bind(tree)
        .execute(pool)
        .await
        .expect("seed");
    }

    async fn update_and_read(
        pool: &PgPool,
        update: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> Option<String> {
        update.execute(pool).await.expect("update");
        sqlx::query_scalar("SELECT tree_digest FROM pack_tarballs WHERE issue_slug = 's'")
            .fetch_one(pool)
            .await
            .expect("read")
    }

    /// The tree digest survives a byte rewrite only when the new bytes are that tree's own
    /// tarball, whether or not the writer names the tree, and an update that leaves the bytes
    /// alone never touches it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_tree_digest_survives_only_its_own_tarball(pool: PgPool) {
        let tree = pack(&[("a", b"1")]);
        let mut conn = pool.acquire().await.expect("conn");
        let digest = put_tree(&mut conn, &encoded(&tree)).await.expect("put");
        let pinned = Some(digest.to_string());

        seed_legacy_row(&pool, digest.as_str()).await;
        let other_column = sqlx::query("UPDATE pack_tarballs SET created_at = 'later'");
        assert_eq!(update_and_read(&pool, other_column).await, pinned);

        seed_legacy_row(&pool, digest.as_str()).await;
        let old_writer = sqlx::query("UPDATE pack_tarballs SET tar_gz = 'other'");
        assert_eq!(update_and_read(&pool, old_writer).await, None);

        seed_legacy_row(&pool, digest.as_str()).await;
        let mislabelled =
            sqlx::query("UPDATE pack_tarballs SET tar_gz = 'other', tree_digest = $1")
                .bind(digest.as_str());
        assert_eq!(update_and_read(&pool, mislabelled).await, None);

        seed_legacy_row(&pool, digest.as_str()).await;
        let own_tarball = sqlx::query("UPDATE pack_tarballs SET tar_gz = $1")
            .bind(tree.tarball().expect("encode"));
        assert_eq!(update_and_read(&pool, own_tarball).await, pinned);
    }

    /// Bytes that conversion recorded as an encoding of the tree keep the tree when written back,
    /// and bytes aliased to a different tree do not.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn bytes_aliased_to_the_tree_keep_it(pool: PgPool) {
        let tree = pack(&[("a", b"1")]);
        let other = pack(&[("a", b"2")]);
        let mut conn = pool.acquire().await.expect("conn");
        let digest = put_tree(&mut conn, &encoded(&tree)).await.expect("put");
        let other_digest = put_tree(&mut conn, &encoded(&other)).await.expect("put");
        for (bytes, aliased_to) in [("converted", &digest), ("elsewhere", &other_digest)] {
            sqlx::query(
                "INSERT INTO pack_digest_aliases (old_digest, tree_digest, recorded_at)
                 VALUES ($1, $2, 'then')",
            )
            .bind(crucible_contract::content_digest(bytes.as_bytes()))
            .bind(aliased_to.as_str())
            .execute(&pool)
            .await
            .expect("alias");
        }
        let pinned = Some(digest.to_string());

        seed_legacy_row(&pool, digest.as_str()).await;
        let converted = sqlx::query("UPDATE pack_tarballs SET tar_gz = 'converted'");
        assert_eq!(update_and_read(&pool, converted).await, pinned);

        seed_legacy_row(&pool, digest.as_str()).await;
        let elsewhere =
            sqlx::query("UPDATE pack_tarballs SET tar_gz = 'elsewhere', tree_digest = $1")
                .bind(digest.as_str());
        assert_eq!(update_and_read(&pool, elsewhere).await, None);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_stored_tree_reads_back_whole_or_one_file(pool: PgPool) {
        let tree = pack(&[("crucible.toml", b"m"), ("tools/run.sh", b"r")]);
        let mut conn = pool.acquire().await.expect("conn");
        let digest = put_tree(&mut conn, &encoded(&tree)).await.expect("put");

        assert_eq!(
            get_tree(&pool, &digest).await.expect("get"),
            Some(tree.clone())
        );
        assert_eq!(
            load(&pool, PackRef::Tree(digest.clone()))
                .await
                .expect("load"),
            tree
        );
        assert_eq!(
            read_file(&pool, &digest, "tools/run.sh")
                .await
                .expect("read"),
            Some(b"r".to_vec())
        );
        assert_eq!(
            read_file(&pool, &digest, "absent").await.expect("read"),
            None
        );
        let missing = pack(&[("b", b"2")]).digest();
        assert_eq!(get_tree(&pool, &missing).await.expect("get"), None);
        let err = load(&pool, PackRef::Tree(missing))
            .await
            .expect_err("unstored")
            .to_string();
        assert!(err.contains("is not stored"), "{err}");

        let empty = PackTree::default();
        let empty_digest = put_tree(&mut conn, &encoded(&empty)).await.expect("put");
        assert_eq!(
            get_tree(&pool, &empty_digest).await.expect("get"),
            Some(empty)
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_tampered_tree_is_never_returned(pool: PgPool) {
        let tree = pack(&[("a", b"1")]);
        let mut conn = pool.acquire().await.expect("conn");
        let digest = put_tree(&mut conn, &encoded(&tree)).await.expect("put");
        sqlx::query("UPDATE pack_tree_files SET content = 'evil' WHERE digest = $1")
            .bind(digest.as_str())
            .execute(&pool)
            .await
            .expect("tamper");

        let err = get_tree(&pool, &digest)
            .await
            .expect_err("tampered")
            .to_string();
        assert!(err.contains("no longer matches"), "{err}");
    }

    /// Every pack reader serves a row a standby wrote (legacy bytes, tree column NULL) by reading
    /// those bytes, and serves the same row after conversion from its stored tree. The two reads
    /// give the same files, though the legacy encoding carried a state dir and other modes.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn every_reader_serves_a_legacy_row_and_its_tree_alike(pool: PgPool) {
        use crate::playbooks::drafts::{DraftSeed, SKELETON_MANIFEST, SKELETON_WORKFLOW};
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("state")).expect("mkdir");
        std::fs::write(dir.path().join("crucible.toml"), SKELETON_MANIFEST).expect("write");
        std::fs::write(dir.path().join("workflow.star"), SKELETON_WORKFLOW).expect("write");
        std::fs::write(dir.path().join("state/session.jsonl"), "s").expect("write");
        let legacy = crate::playbooks::packs::tar_pack_tree(dir.path()).expect("legacy tar");
        let expected = pack(&[
            ("crucible.toml", SKELETON_MANIFEST.as_bytes()),
            ("workflow.star", SKELETON_WORKFLOW.as_bytes()),
        ]);
        assert_ne!(legacy, expected.tarball().expect("encode"));

        crate::playbooks::drafts::create(
            &pool,
            "studio",
            "d",
            DraftSeed::Skeleton,
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("create");
        sqlx::query("UPDATE playbook_draft_versions SET tar_gz = $1 WHERE draft_id = 'studio'")
            .bind(&legacy)
            .execute(&pool)
            .await
            .expect("a standby rewrites the bytes");
        sqlx::query(
            "INSERT INTO playbooks (id, description, repo, rev, path, tar_gz, tar_digest, \
             tar_bytes, params_schema, schema_digest, core_rev, created_at, updated_at) \
             VALUES ('survey', 'd', 'o/r', 'aaa', 'packs/survey', $1, 'sha256:1', 1, '{}'::jsonb, \
             'sha256:2', 'pin', 'now', 'now')",
        )
        .bind(&legacy)
        .execute(&pool)
        .await
        .expect("seed registry");
        sqlx::query(
            "INSERT INTO pack_imports (id, repo, git_ref, path, rev, tar_gz, tar_digest, \
             tar_bytes, diagnostics, core_rev, status, created_at) \
             VALUES ('imp-1', 'o/other', 'main', 'packs/audit', 'ccc', $1, 'sha256:3', 1, \
             '[]'::jsonb, 'pin', 'pending', 'now')",
        )
        .bind(&legacy)
        .execute(&pool)
        .await
        .expect("seed import");
        sqlx::query(
            "INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at)
             VALUES ('owner_repo_7', $1, $2, 1, 'now')",
        )
        .bind(&legacy)
        .bind(crucible_contract::content_digest(&legacy))
        .execute(&pool)
        .await
        .expect("seed launch pack");

        let unconverted: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM playbooks WHERE tree_digest IS NULL)
                  + (SELECT count(*) FROM pack_imports WHERE tree_digest IS NULL)
                  + (SELECT count(*) FROM playbook_draft_versions WHERE tree_digest IS NULL)
                  + (SELECT count(*) FROM pack_tarballs WHERE tree_digest IS NULL)",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(unconverted, 4, "every row is a standby's");

        for converted in [false, true] {
            if converted {
                let report = crate::playbooks::pack_migration::convert_pack_trees(&pool)
                    .await
                    .expect("convert");
                assert_eq!(report.converted, 4);
            }
            let reads = [
                crate::playbooks::registry::pack(&pool, "survey").await,
                crate::playbooks::imports::pack(&pool, "imp-1").await,
                crate::playbooks::drafts::version_tree(&pool, "studio", Some(1))
                    .await
                    .map(|v| v.map(|(_, tree)| tree)),
                crate::runs::blob_store::get_pack(&pool, "owner_repo_7").await,
            ];
            for (i, read) in reads.into_iter().enumerate() {
                assert_eq!(
                    read.expect("read"),
                    Some(expected.clone()),
                    "reader {i}, converted {converted}"
                );
            }
            let pack = crate::playbooks::packs::materialize_pack(&pool, "owner/repo#7")
                .await
                .expect("materialize")
                .expect("stored");
            assert_eq!(
                crucible_contract::pack_tree::walk_dir(pack.path())
                    .expect("walk")
                    .tree,
                expected,
                "converted {converted}"
            );
            #[cfg(feature = "autoresearch")]
            assert_eq!(
                crate::playbooks::packs::read_pack_file(&pool, "owner/repo#7", "workflow.star")
                    .await
                    .expect("read"),
                Some(SKELETON_WORKFLOW.to_string()),
                "converted {converted}"
            );
        }
    }

    /// Legacy bytes that cannot be a tree load as an [`Unconvertible`] refusal naming the reason,
    /// whether conversion recorded them or a standby wrote them since.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unconvertible_legacy_bytes_load_as_a_refusal(pool: PgPool) {
        let tgz = crate::testing::symlink_pack();
        let expected = Unconvertible {
            digest: crucible_contract::content_digest(&tgz),
            reason: crate::testing::SYMLINK_REASON.to_string(),
        };
        for recorded in [false, true] {
            if recorded {
                sqlx::query(
                    "INSERT INTO pack_digest_aliases (old_digest, unconvertible_reason, recorded_at)
                     VALUES ($1, $2, 'now')",
                )
                .bind(&expected.digest)
                .bind(&expected.reason)
                .execute(&pool)
                .await
                .expect("alias");
            }
            assert_eq!(
                unconvertible(&pool, &tgz).await.expect("lookup"),
                recorded.then(|| expected.clone())
            );
            let err = load(&pool, PackRef::Legacy(tgz.clone()))
                .await
                .expect_err("refused");
            assert_eq!(
                err.downcast_ref::<Unconvertible>(),
                Some(&expected),
                "recorded {recorded}"
            );
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_supplied_digest_resolves_to_what_it_names(pool: PgPool) {
        let mut conn = pool.acquire().await.expect("conn");
        let digest = put_tree(&mut conn, &encoded(&pack(&[("a", b"1")])))
            .await
            .expect("put");
        sqlx::query(
            "INSERT INTO pack_digest_aliases (old_digest, tree_digest, unconvertible_reason, recorded_at)
             VALUES ('sha256:old', $1, NULL, 'now'), ('sha256:broken', NULL, 'a symlink', 'now')",
        )
        .bind(digest.as_str())
        .execute(&pool)
        .await
        .expect("aliases");
        let missing = pack(&[("b", b"2")]).digest();

        assert_eq!(
            resolve_supplied(&pool, digest.as_str())
                .await
                .expect("current"),
            Resolved::Current(digest.clone())
        );
        assert_eq!(
            resolve_supplied(&pool, "sha256:old").await.expect("old"),
            Resolved::Superseded {
                replacement: digest.clone()
            }
        );
        assert_eq!(
            resolve_supplied(&pool, "sha256:broken")
                .await
                .expect("broken"),
            Resolved::Unconvertible {
                reason: "a symlink".to_string()
            }
        );
        for unknown in [missing.as_str(), "sha256:never", "abc123"] {
            assert_eq!(
                resolve_supplied(&pool, unknown).await.expect("unknown"),
                Resolved::Unknown,
                "{unknown}"
            );
        }

        assert_eq!(
            superseded_in(&pool, "p", "sha256:old")
                .await
                .expect("none held"),
            None,
            "a playbook that never held the replacement learns nothing"
        );
        sqlx::query(
            "INSERT INTO playbooks (id, description, repo, rev, path, tar_gz, tar_digest, \
             tar_bytes, params_schema, schema_digest, core_rev, created_at, updated_at) \
             VALUES ('p', 'd', 'o/r', 'aaa', '', 'x', 'sha256:x', 1, '{}'::jsonb, 's', 'c', \
             'now', 'now')",
        )
        .execute(&pool)
        .await
        .expect("playbook");
        sqlx::query(
            "INSERT INTO playbook_revisions (playbook_id, tree_digest, first_seen_at) \
             VALUES ('p', $1, 'now')",
        )
        .bind(digest.as_str())
        .execute(&pool)
        .await
        .expect("revision");
        assert_eq!(
            superseded_in(&pool, "p", "sha256:old").await.expect("held"),
            Some(digest.clone())
        );
        assert_eq!(
            superseded_in(&pool, "p", digest.as_str())
                .await
                .expect("current"),
            None
        );
        assert_eq!(
            superseded_in(&pool, "p", "sha256:broken")
                .await
                .expect("broken"),
            None
        );
    }

    async fn age_every_tree(pool: &PgPool) {
        sqlx::query("UPDATE pack_trees SET created_at = '2000-01-01T00:00:00Z'")
            .execute(pool)
            .await
            .expect("age");
    }

    async fn stored(pool: &PgPool, digest: &str) -> (bool, i64) {
        sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM pack_trees WHERE digest = $1),
                    (SELECT count(*) FROM pack_tree_files WHERE digest = $1)",
        )
        .bind(digest)
        .fetch_one(pool)
        .await
        .expect("stored")
    }

    async fn skeleton_draft(pool: &PgPool, id: &str) -> String {
        crate::playbooks::drafts::create(
            pool,
            id,
            "d",
            crate::playbooks::drafts::DraftSeed::Skeleton,
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("draft");
        sqlx::query_scalar(
            "SELECT tree_digest FROM playbook_draft_versions WHERE draft_id = $1 AND version = 1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("draft tree")
    }

    /// Every foreign key into `pack_trees` other than its own files is a pin collection honors.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn collection_honors_every_foreign_key_into_pack_trees(pool: PgPool) {
        let mut keys: Vec<(String, String)> = sqlx::query_as(
            "SELECT cl.relname::TEXT, a.attname::TEXT FROM pg_constraint c
             JOIN pg_class cl ON cl.oid = c.conrelid
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey)
             WHERE c.contype = 'f' AND c.confrelid = 'pack_trees'::regclass
               AND cl.relname <> 'pack_tree_files'",
        )
        .fetch_all(&pool)
        .await
        .expect("foreign keys");
        keys.sort();
        let mut pins: Vec<(String, String)> = TREE_PINS
            .iter()
            .map(|(t, c)| (t.to_string(), c.to_string()))
            .collect();
        pins.sort();
        assert_eq!(keys, pins);
    }

    /// Deleting a draft leaves its tree while another draft holds the same tree, and the tree and
    /// its files go once the last draft holding it is deleted.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_deleted_drafts_tree_goes_only_when_nothing_else_pins_it(pool: PgPool) {
        let first = skeleton_draft(&pool, "first").await;
        let second = skeleton_draft(&pool, "second").await;
        assert_eq!(first, second);
        let (_, files) = stored(&pool, &first).await;
        assert!(files > 0);
        age_every_tree(&pool).await;

        assert!(
            crate::playbooks::drafts::delete(&pool, "first")
                .await
                .expect("delete")
        );
        assert_eq!(collect(&pool).await.expect("collect"), 0);
        assert_eq!(stored(&pool, &first).await, (true, files));

        assert!(
            crate::playbooks::drafts::delete(&pool, "second")
                .await
                .expect("delete")
        );
        assert_eq!(collect(&pool).await.expect("collect"), 1);
        assert_eq!(stored(&pool, &first).await, (false, 0));
        assert_eq!(collect(&pool).await.expect("again"), 0);
    }

    /// A launch's frozen copy of a draft version pins its tree after the draft is gone, and an
    /// alias naming the tree does not.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_tree_a_launch_pins_survives_its_draft(pool: PgPool) {
        let digest = skeleton_draft(&pool, "studio").await;
        assert!(
            crate::playbooks::drafts::copy_draft_pack_to(&pool, "studio", 1, "launch_1")
                .await
                .expect("copy")
        );
        sqlx::query(
            "INSERT INTO pack_digest_aliases (old_digest, tree_digest, recorded_at)
             VALUES ('sha256:old', $1, 'then')",
        )
        .bind(&digest)
        .execute(&pool)
        .await
        .expect("alias");
        assert!(
            crate::playbooks::drafts::delete(&pool, "studio")
                .await
                .expect("delete")
        );
        age_every_tree(&pool).await;

        assert_eq!(collect(&pool).await.expect("collect"), 0);
        assert!(stored(&pool, &digest).await.0);

        sqlx::query("DELETE FROM pack_tarballs WHERE issue_slug = 'launch_1'")
            .execute(&pool)
            .await
            .expect("drop the launch copy");
        assert_eq!(collect(&pool).await.expect("collect"), 1);
        assert!(!stored(&pool, &digest).await.0);
    }

    /// An unpinned tree younger than the cutoff survives collection.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_fresh_unpinned_tree_survives(pool: PgPool) {
        let mut conn = pool.acquire().await.expect("conn");
        let fresh = put_tree(&mut conn, &encoded(&pack(&[("a", b"1")])))
            .await
            .expect("put");
        let stamp = |ago: jiff::SignedDuration| {
            crate::clock::stamp(jiff::Timestamp::now().checked_sub(ago).expect("past"))
        };
        sqlx::query("UPDATE pack_trees SET created_at = $1 WHERE digest = $2")
            .bind(stamp(COLLECT_AFTER - jiff::SignedDuration::from_mins(5)))
            .bind(fresh.as_str())
            .execute(&pool)
            .await
            .expect("inside the cutoff");

        assert_eq!(collect(&pool).await.expect("collect"), 0);
        assert!(stored(&pool, fresh.as_str()).await.0);

        sqlx::query("UPDATE pack_trees SET created_at = $1 WHERE digest = $2")
            .bind(stamp(COLLECT_AFTER + jiff::SignedDuration::from_mins(5)))
            .bind(fresh.as_str())
            .execute(&pool)
            .await
            .expect("past the cutoff");
        assert_eq!(collect(&pool).await.expect("collect"), 1);
    }

    /// A writer that reuses an old unpinned tree holds it against collection: a collection that
    /// starts meanwhile waits, then fails on the foreign key once the writer pins the tree, and
    /// the tree and its files stay.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_tree_pinned_while_collecting_fails_the_collection(pool: PgPool) {
        let tree = pack(&[("a", b"1")]);
        let digest = put_tree(&mut pool.acquire().await.expect("conn"), &encoded(&tree))
            .await
            .expect("put");
        age_every_tree(&pool).await;

        let mut writer = pool.begin().await.expect("begin");
        assert_eq!(
            put_tree(&mut writer, &encoded(&tree)).await.expect("reuse"),
            digest
        );
        let collecting = tokio::spawn({
            let pool = pool.clone();
            async move { collect(&pool).await }
        });
        let mut waited = 0;
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND query LIKE 'DELETE FROM pack_trees%'",
            )
            .fetch_one(&pool)
            .await
            .expect("waiting");
            if waiting > 0 {
                break;
            }
            waited += 1;
            assert!(waited < 500, "collection never waited on the writer");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        sqlx::query(
            "INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at, tree_digest)
             VALUES ('launch_1', $1, $2, 1, 'now', $3)",
        )
        .bind(tree.tarball().expect("encode"))
        .bind(crucible_contract::content_digest(
            &tree.tarball().expect("encode"),
        ))
        .bind(digest.as_str())
        .execute(&mut *writer)
        .await
        .expect("pin");
        writer.commit().await.expect("commit");

        let err = collecting
            .await
            .expect("join")
            .expect_err("the pin wins")
            .to_string();
        assert!(err.contains("deleting unpinned pack trees"), "{err}");
        assert_eq!(stored(&pool, digest.as_str()).await, (true, 1));
    }
}

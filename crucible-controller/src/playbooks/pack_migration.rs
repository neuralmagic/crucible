//! Startup conversion of legacy pack bytes to stored trees (RFC-0002:C-PACK-BASE, ADR-0059).
//!
//! Every row that holds a gzipped pack and no tree digest is read as a tree, the tree is stored,
//! the row's old gzip digest is recorded as an alias, and the row is pointed at the tree. A
//! tarball that cannot be a pack is recorded as unconvertible and never retried. Pins that hold
//! an old digest are then rewritten to its tree. Safe to run on every start: converted rows are
//! skipped, and each step is idempotent.

#![allow(clippy::disallowed_macros)]

use crate::playbooks::pack_trees::put_tree;
use anyhow::{Context, Result};
use crucible_contract::content_digest;
use crucible_contract::pack_tree::{TreeDigest, read_tar_gz};
use futures_util::TryStreamExt;
use sqlx::{PgConnection, PgPool, Row};

/// What one conversion pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConversionReport {
    pub converted: usize,
    pub unconvertible: usize,
    pub pins_rewritten: u64,
}

/// A legacy pack-byte column, its tree column, and a SQL expression naming one row as text.
struct Column {
    table: &'static str,
    key: &'static str,
    bytes: &'static str,
    tree: &'static str,
}

const COLUMNS: [Column; 5] = [
    Column {
        table: "playbooks",
        key: "id",
        bytes: "tar_gz",
        tree: "tree_digest",
    },
    Column {
        table: "pack_imports",
        key: "id",
        bytes: "tar_gz",
        tree: "tree_digest",
    },
    Column {
        table: "playbook_draft_versions",
        key: "draft_id || '/' || version",
        bytes: "tar_gz",
        tree: "tree_digest",
    },
    Column {
        table: "playbook_standing_launches",
        key: "id",
        bytes: "adopted_tar_gz",
        tree: "adopted_tree_digest",
    },
    Column {
        table: "pack_tarballs",
        key: "issue_slug",
        bytes: "tar_gz",
        tree: "tree_digest",
    },
];

/// Pin columns that may hold an old gzip pack digest. Only values with an alias are rewritten, so
/// a git commit or an engine identity in the same column is left alone.
const PINS: [(&str, &str); 6] = [
    ("playbooks", "rev"),
    ("playbook_standing_launches", "adopted_rev"),
    ("playbook_drafts", "origin_rev"),
    ("secret_bindings", "pack_rev"),
    ("scopes", "pack_digest"),
    ("runs", "identity_digest"),
];

/// What an old gzip digest became.
enum AliasTarget {
    Tree(TreeDigest),
    Unconvertible(String),
}

/// SQL for the `content_digest` of a bytea column, so a guard compares like with like.
fn content_digest_sql(column: &str) -> String {
    format!("'sha256:' || encode(sha256({column}), 'hex')")
}

/// Convert every unconverted legacy pack row and rewrite the pins that name an old digest. The
/// caller holds the maintenance lock, so this is the only writer of tree columns.
pub async fn convert_pack_trees(pool: &PgPool) -> Result<ConversionReport> {
    let mut report = ConversionReport::default();
    for column in &COLUMNS {
        convert_column(pool, column, &mut report).await?;
    }
    for (table, pin) in PINS {
        report.pins_rewritten += sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} t SET {pin} = a.tree_digest FROM pack_digest_aliases a
             WHERE t.{pin} = a.old_digest AND a.tree_digest IS NOT NULL"
        )))
        .execute(pool)
        .await
        .with_context(|| format!("rewriting {table}.{pin} to tree digests"))?
        .rows_affected();
    }
    sqlx::query(
        "INSERT INTO playbook_revisions (playbook_id, tree_digest, first_seen_at)
         SELECT id, tree_digest, $1 FROM playbooks WHERE tree_digest IS NOT NULL
         ON CONFLICT DO NOTHING",
    )
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("recording each playbook's current tree as a revision")?;
    Ok(report)
}

async fn convert_column(pool: &PgPool, c: &Column, report: &mut ConversionReport) -> Result<()> {
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT ({key})::TEXT AS row_key, {bytes} AS bytes FROM {table} t
         WHERE t.{tree} IS NULL AND t.{bytes} IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM pack_digest_aliases a
               WHERE a.old_digest = {digest} AND a.unconvertible_reason IS NOT NULL)",
        key = c.key,
        table = c.table,
        tree = c.tree,
        bytes = c.bytes,
        digest = content_digest_sql(&format!("t.{}", c.bytes)),
    )))
    .fetch(pool);
    while let Some(row) = rows
        .try_next()
        .await
        .with_context(|| format!("reading unconverted {} rows", c.table))?
    {
        let key: String = row.try_get("row_key")?;
        let bytes: Vec<u8> = row.try_get("bytes")?;
        let old = content_digest(&bytes);
        let mut tx = pool.begin().await?;
        let known: Option<String> = sqlx::query_scalar(
            "SELECT tree_digest FROM pack_digest_aliases WHERE old_digest = $1 AND tree_digest IS NOT NULL",
        )
        .bind(&old)
        .fetch_optional(&mut *tx)
        .await?;
        let target = match known {
            Some(tree) => AliasTarget::Tree(tree.parse().map_err(anyhow::Error::msg)?),
            None => match tokio::task::spawn_blocking(move || read_tar_gz(&bytes))
                .await
                .context("joining the pack conversion worker")?
            {
                Ok(read) => AliasTarget::Tree(put_tree(&mut tx, &read.tree).await?),
                Err(reason) => AliasTarget::Unconvertible(reason.to_string()),
            },
        };
        record_alias(&mut tx, &old, &target).await?;
        match &target {
            AliasTarget::Tree(tree) => {
                pin_tree(&mut tx, c, &key, tree, &old).await?;
                report.converted += 1;
            }
            AliasTarget::Unconvertible(reason) => {
                tracing::warn!(table = c.table, key, %reason, "pack cannot be a tree; recorded unconvertible");
                report.unconvertible += 1;
            }
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Point one row at `tree`, but only while its bytes still have the digest `old`: a controller
/// that predates trees may have rewritten them since they were read. Returns the rows updated.
async fn pin_tree(
    conn: &mut PgConnection,
    c: &Column,
    key: &str,
    tree: &TreeDigest,
    old: &str,
) -> Result<u64> {
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET {tree_col} = $2 WHERE ({key})::TEXT = $1 AND {digest} = $3",
        table = c.table,
        tree_col = c.tree,
        key = c.key,
        digest = content_digest_sql(c.bytes),
    )))
    .bind(key)
    .bind(tree.as_str())
    .bind(old)
    .execute(conn)
    .await
    .with_context(|| format!("pinning {} {key} to its tree", c.table))?
    .rows_affected())
}

async fn record_alias(conn: &mut PgConnection, old: &str, target: &AliasTarget) -> Result<()> {
    let (tree, reason) = match target {
        AliasTarget::Tree(tree) => (Some(tree.as_str()), None),
        AliasTarget::Unconvertible(reason) => (None, Some(reason.as_str())),
    };
    sqlx::query(
        "INSERT INTO pack_digest_aliases (old_digest, tree_digest, unconvertible_reason, recorded_at)
         VALUES ($1, $2, $3, $4) ON CONFLICT (old_digest) DO NOTHING",
    )
    .bind(old)
    .bind(tree)
    .bind(reason)
    .bind(crate::clock::now_rfc3339())
    .execute(conn)
    .await
    .context("recording a pack digest alias")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::playbooks::pack_migration::*;
    use crucible_contract::pack_tree::PackTree;

    fn pack(files: &[(&str, &[u8])]) -> PackTree {
        PackTree::from_pairs(files).expect("tree")
    }

    async fn seed_launch_pack(pool: &PgPool, slug: &str, tar_gz: &[u8]) {
        sqlx::query(
            "INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at)
             VALUES ($1, $2, $3, $4, 'now')",
        )
        .bind(slug)
        .bind(tar_gz)
        .bind(content_digest(tar_gz))
        .bind(tar_gz.len() as i64)
        .execute(pool)
        .await
        .expect("seed pack_tarballs");
    }

    async fn tree_of(pool: &PgPool, slug: &str) -> Option<String> {
        sqlx::query_scalar("SELECT tree_digest FROM pack_tarballs WHERE issue_slug = $1")
            .bind(slug)
            .fetch_one(pool)
            .await
            .expect("read")
    }

    /// What an older controller stored, `tar_pack_tree` of a directory with a state dir and a
    /// file mode, converts to the tree a walk of the same directory gives, and a second pass
    /// finds nothing to do.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn legacy_tarballs_become_trees_and_conversion_is_idempotent(pool: PgPool) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("state")).expect("mkdir");
        std::fs::write(dir.path().join("crucible.toml"), "m").expect("write");
        std::fs::write(dir.path().join("run.sh"), "r").expect("write");
        std::fs::set_permissions(
            dir.path().join("run.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        std::fs::write(dir.path().join("state/s"), "s").expect("write");
        let legacy = crate::playbooks::packs::tar_pack_tree(dir.path()).expect("tar");
        seed_launch_pack(&pool, "a", &legacy).await;

        let report = convert_pack_trees(&pool).await.expect("convert");

        let walked = crucible_contract::pack_tree::walk_dir(dir.path())
            .expect("walk")
            .tree
            .digest();
        assert_eq!(report.converted, 1);
        assert_eq!(tree_of(&pool, "a").await.as_deref(), Some(walked.as_str()));
        let alias: Option<String> =
            sqlx::query_scalar("SELECT tree_digest FROM pack_digest_aliases WHERE old_digest = $1")
                .bind(content_digest(&legacy))
                .fetch_one(&pool)
                .await
                .expect("alias");
        assert_eq!(alias.as_deref(), Some(walked.as_str()));

        let again = convert_pack_trees(&pool).await.expect("again");
        assert_eq!(again, ConversionReport::default());
    }

    /// Two archives of the same files, different only in gzip and metadata, store one tree and
    /// keep an alias for each old digest.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn differently_encoded_copies_share_one_tree(pool: PgPool) {
        let tree = pack(&[("crucible.toml", b"m")]);
        let a = tree.tarball().expect("encode");
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("crucible.toml"), "m").expect("write");
        let b = crate::playbooks::packs::tar_pack_tree(dir.path()).expect("tar");
        assert_ne!(a, b);
        seed_launch_pack(&pool, "a", &a).await;
        seed_launch_pack(&pool, "b", &b).await;

        convert_pack_trees(&pool).await.expect("convert");

        let counts: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM pack_trees), (SELECT count(*) FROM pack_digest_aliases)",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(counts, (1, 2));
        assert_eq!(tree_of(&pool, "a").await, tree_of(&pool, "b").await);
    }

    /// A tarball holding a symlink is recorded unconvertible with the reason, keeps no tree, and
    /// is not retried.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unconvertible_tarball_is_recorded_once(pool: PgPool) {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "link", "crucible.toml")
            .expect("link");
        let tgz = builder.into_inner().expect("tar").finish().expect("gzip");
        seed_launch_pack(&pool, "bad", &tgz).await;

        let report = convert_pack_trees(&pool).await.expect("convert");

        assert_eq!((report.converted, report.unconvertible), (0, 1));
        assert_eq!(tree_of(&pool, "bad").await, None);
        let reason: Option<String> = sqlx::query_scalar(
            "SELECT unconvertible_reason FROM pack_digest_aliases WHERE old_digest = $1",
        )
        .bind(content_digest(&tgz))
        .fetch_one(&pool)
        .await
        .expect("alias");
        assert!(reason.expect("reason").contains("link"));
        assert_eq!(
            convert_pack_trees(&pool).await.expect("again"),
            ConversionReport::default()
        );
    }

    /// A pin holding an old gzip digest is rewritten to the tree; a git commit or an engine
    /// identity in the same column is left alone.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn pins_naming_an_old_digest_are_rewritten(pool: PgPool) {
        let tree = pack(&[("crucible.toml", b"m")]);
        let legacy = tree.tarball().expect("encode");
        let old = content_digest(&legacy);
        seed_launch_pack(&pool, "a", &legacy).await;
        sqlx::query(
            "INSERT INTO playbook_drafts (id, description, created_by, created_at, updated_at, origin_rev)
             VALUES ('d1', 'x', 'w', 'now', 'now', $1), ('d2', 'x', 'w', 'now', 'now', 'abc123')",
        )
        .bind(&old)
        .execute(&pool)
        .await
        .expect("seed drafts");

        let report = convert_pack_trees(&pool).await.expect("convert");

        let revs: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, origin_rev FROM playbook_drafts ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("read");
        assert_eq!(
            revs,
            vec![
                ("d1".to_string(), Some(tree.digest().to_string())),
                ("d2".to_string(), Some("abc123".to_string())),
            ]
        );
        assert_eq!(report.pins_rewritten, 1);
    }

    /// An older controller that rewrites the bytes of a row while it is being converted does not
    /// get a tree digest pinned to bytes it no longer holds.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_row_rewritten_after_it_was_read_is_not_pinned(pool: PgPool) {
        let first = pack(&[("a", b"1")]).tarball().expect("encode");
        let second = pack(&[("a", b"2")]).tarball().expect("encode");
        seed_launch_pack(&pool, "a", &second).await;

        let mut tx = pool.begin().await.expect("tx");
        let stale = put_tree(&mut tx, &pack(&[("a", b"1")])).await.expect("put");
        let launch_packs = COLUMNS
            .iter()
            .find(|c| c.table == "pack_tarballs")
            .expect("pack_tarballs column");
        let updated = pin_tree(&mut tx, launch_packs, "a", &stale, &content_digest(&first))
            .await
            .expect("guarded update");
        tx.commit().await.expect("commit");

        assert_eq!(
            updated, 0,
            "the bytes changed, so the stale tree is not pinned"
        );
        assert_eq!(tree_of(&pool, "a").await, None);
    }

    /// Every legacy column the conversion walks has its tree column and the trigger that clears it
    /// when an older controller rewrites the bytes, so the list and migration 0051 cannot drift.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn every_converted_column_exists_with_its_trigger(pool: PgPool) {
        for c in &COLUMNS {
            let columns: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM information_schema.columns
                 WHERE table_name = $1 AND column_name IN ($2, $3)",
            )
            .bind(c.table)
            .bind(c.bytes)
            .bind(c.tree)
            .fetch_one(&pool)
            .await
            .expect("columns");
            assert_eq!(columns, 2, "{}: {} and {}", c.table, c.bytes, c.tree);
            let trigger: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_trigger WHERE tgname = $1 || '_legacy_bytes'",
            )
            .bind(c.table)
            .fetch_one(&pool)
            .await
            .expect("trigger");
            assert_eq!(trigger, 1, "{} has no legacy-bytes trigger", c.table);
        }
    }
}

//! Startup conversion of legacy pack bytes to stored trees (RFC-0002:C-PACK-BASE, ADR-0059).
//!
//! Every row that holds a gzipped pack and no tree digest is read as a tree, the tree is stored,
//! the row's old gzip digest is recorded as an alias, and the row is pointed at the tree. A
//! tarball that cannot be a pack is recorded as unconvertible and never retried. Then, in one
//! transaction, the comparison pins are derived from what the rows held when they were pinned and
//! a draft-sourced playbook's rev becomes its tree digest, the value its writer now mints. Safe to
//! run on every start: converted rows are skipped, and each step is idempotent.

#![allow(clippy::disallowed_macros)]

use crate::playbooks::pack_trees::{EncodedPack, put_tree};
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
    /// Pin columns derived and draft-sourced revs rewritten.
    pub pinned: usize,
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

/// What an old gzip digest became.
enum AliasTarget {
    Tree(TreeDigest),
    Unconvertible(String),
}

/// SQL for the `content_digest` of a bytea column, so a guard compares like with like.
fn content_digest_sql(column: &str) -> String {
    format!("'sha256:' || encode(sha256({column}), 'hex')")
}

/// Convert every unconverted legacy pack row. The caller holds the maintenance lock, so this is
/// the only writer of tree columns.
pub async fn convert_pack_trees(pool: &PgPool) -> Result<ConversionReport> {
    let mut report = ConversionReport::default();
    for column in &COLUMNS {
        convert_column(pool, column, &mut report).await?;
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO playbook_revisions (playbook_id, tree_digest, first_seen_at)
         SELECT id, tree_digest, $1 FROM playbooks WHERE tree_digest IS NOT NULL
         ON CONFLICT DO NOTHING",
    )
    .bind(crate::clock::now_rfc3339())
    .execute(&mut *tx)
    .await
    .context("recording each playbook's current tree as a revision")?;
    for (what, statement) in PINS {
        let updated = sqlx::query(statement)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("deriving {what}"))?
            .rows_affected();
        report.pinned += usize::try_from(updated).context("pinned row count")?;
    }
    tx.commit().await?;
    Ok(report)
}

/// Each unset pin, taken from the origin's tree when the stored rev is still the origin's rev or
/// from the alias of a pre-tree rev; then each draft-sourced rev that is still a pre-tree digest.
const PINS: [(&str, &str); 6] = [
    (
        "draft origin pins from the origin playbook",
        "UPDATE playbook_drafts d SET origin_digest = p.tree_digest FROM playbooks p
         WHERE d.origin_digest IS NULL AND d.origin_playbook = p.id AND d.origin_rev = p.rev
           AND p.tree_digest IS NOT NULL",
    ),
    (
        "draft origin pins from the origin import",
        "UPDATE playbook_drafts d SET origin_digest = i.tree_digest FROM pack_imports i
         WHERE d.origin_digest IS NULL AND d.origin_import = i.id AND i.tree_digest IS NOT NULL",
    ),
    (
        "draft origin pins from a pre-tree rev",
        "UPDATE playbook_drafts d SET origin_digest = a.tree_digest FROM pack_digest_aliases a
         WHERE d.origin_digest IS NULL AND d.origin_rev = a.old_digest
           AND a.tree_digest IS NOT NULL",
    ),
    (
        "secret binding pins from the bound playbook",
        "UPDATE secret_bindings b SET pack_digest = p.tree_digest FROM playbooks p
         WHERE b.pack_digest IS NULL AND b.scope_kind = 'playbook' AND b.scope_id = p.id
           AND b.pack_rev = p.rev AND p.tree_digest IS NOT NULL",
    ),
    (
        "secret binding pins from a pre-tree rev",
        "UPDATE secret_bindings b SET pack_digest = a.tree_digest FROM pack_digest_aliases a
         WHERE b.pack_digest IS NULL AND b.scope_kind = 'playbook' AND b.pack_rev = a.old_digest
           AND a.tree_digest IS NOT NULL",
    ),
    (
        "draft-sourced revs",
        "UPDATE playbooks p SET rev = p.tree_digest FROM pack_digest_aliases a
         WHERE p.source_draft IS NOT NULL AND p.rev = a.old_digest
           AND a.tree_digest = p.tree_digest",
    ),
];

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
            "SELECT a.tree_digest FROM pack_digest_aliases a
             JOIN pack_trees t ON t.digest = a.tree_digest
             WHERE a.old_digest = $1
             FOR KEY SHARE OF t",
        )
        .bind(&old)
        .fetch_optional(&mut *tx)
        .await?;
        let target = match known {
            Some(tree) => AliasTarget::Tree(tree.parse().map_err(anyhow::Error::msg)?),
            None => match tokio::task::spawn_blocking(move || {
                read_tar_gz(&bytes).map(|read| EncodedPack::new(read.tree))
            })
            .await
            .context("joining the pack conversion worker")?
            {
                Ok(pack) => {
                    let pack = pack.context("encoding the pack tarball")?;
                    AliasTarget::Tree(put_tree(&mut tx, &pack).await?)
                }
                Err(reason) => AliasTarget::Unconvertible(reason.to_string()),
            },
        };
        record_alias(&mut tx, &old, &target).await?;
        match &target {
            AliasTarget::Tree(tree) => {
                if pin_tree(&mut tx, c, &key, tree, &old).await? == 1 {
                    report.converted += 1;
                }
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

    /// Legacy bytes whose alias names a collected tree convert by storing that tree again.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn bytes_whose_aliased_tree_was_collected_store_it_again(pool: PgPool) {
        let legacy = pack(&[("crucible.toml", b"m")]).tarball().expect("encode");
        seed_launch_pack(&pool, "a", &legacy).await;
        convert_pack_trees(&pool).await.expect("convert");
        let tree = tree_of(&pool, "a").await.expect("pinned");
        sqlx::query("DELETE FROM pack_tarballs")
            .execute(&pool)
            .await
            .expect("drop the pin");
        sqlx::query("UPDATE pack_trees SET created_at = '2000-01-01T00:00:00Z'")
            .execute(&pool)
            .await
            .expect("age");
        assert_eq!(
            crate::playbooks::pack_trees::collect(&pool)
                .await
                .expect("collect")
                .trees,
            1
        );

        seed_launch_pack(&pool, "b", &legacy).await;
        let report = convert_pack_trees(&pool).await.expect("convert again");

        assert_eq!(report.converted, 1);
        assert_eq!(tree_of(&pool, "b").await.as_deref(), Some(tree.as_str()));
        let stored: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM pack_trees WHERE digest = $1),
                    (SELECT count(*) FROM pack_tree_files WHERE digest = $1)",
        )
        .bind(&tree)
        .fetch_one(&pool)
        .await
        .expect("stored");
        assert_eq!(stored, (1, 1));
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

    /// Every legacy column converts, the composite draft-version key included. A git-sourced
    /// rev, an adopted rev and an origin rev naming the old digest stay as they were, and the
    /// draft's origin pin is taken from the old digest's alias.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn every_legacy_column_converts_and_revs_other_than_draft_sources_stay(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("crucible.toml"), "m").expect("write");
        let legacy = crate::playbooks::packs::tar_pack_tree(dir.path()).expect("tar");
        let old = content_digest(&legacy);
        let size = legacy.len() as i64;
        seed_launch_pack(&pool, "a", &legacy).await;
        for statement in [
            "INSERT INTO playbook_drafts (id, description, created_at, updated_at, origin_rev)
             VALUES ('d', 'x', 'now', 'now', $3)",
            "INSERT INTO playbook_draft_versions
                 (draft_id, version, tar_gz, tar_digest, tar_bytes, diagnostics, core_rev, created_at)
             VALUES ('d', 2, $1, $3, $2, '[]', 'c', 'now')",
            "INSERT INTO playbooks (id, description, repo, path, rev, tar_gz, tar_digest, tar_bytes,
                 params_schema, schema_digest, core_rev, created_at, updated_at)
             VALUES ('p', 'x', 'o/r', '.', $3, $1, $3, $2, '{}', 's', 'c', 'now', 'now')",
            "INSERT INTO pack_imports (id, repo, path, rev, tar_gz, tar_digest, tar_bytes,
                 diagnostics, core_rev, status, created_at)
             VALUES ('i', 'o/r', '.', 'abc', $1, $3, $2, '[]', 'c', 'pending', 'now')",
            "INSERT INTO playbook_standing_launches (id, trigger, playbook, params, schema_digest,
                 max_cost, max_time, created_at, updated_at, target_kind, adopted_rev,
                 adopted_tar_gz, adopted_tar_digest, adopted_tar_bytes, adopted_params_schema)
             VALUES ('s', 'schedule', 'p', '{}', 's', 1, '5m', 'now', 'now', 'adopted', $3,
                 $1, $3, $2, '{}')",
        ] {
            sqlx::query(statement)
                .bind(&legacy)
                .bind(size)
                .bind(&old)
                .execute(&pool)
                .await
                .expect(statement);
        }

        let report = convert_pack_trees(&pool).await.expect("convert");

        let tree = pack(&[("crucible.toml", b"m")]).digest().to_string();
        assert_eq!(
            report,
            ConversionReport {
                converted: 5,
                unconvertible: 0,
                pinned: 1,
            }
        );
        let trees: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT tree_digest FROM playbooks
             UNION ALL SELECT tree_digest FROM pack_imports
             UNION ALL SELECT tree_digest FROM playbook_draft_versions
             UNION ALL SELECT adopted_tree_digest FROM playbook_standing_launches
             UNION ALL SELECT tree_digest FROM pack_tarballs",
        )
        .fetch_all(&pool)
        .await
        .expect("trees");
        assert_eq!(trees, vec![Some(tree.clone()); 5]);
        let pins: Vec<String> = sqlx::query_scalar(
            "SELECT rev FROM playbooks
             UNION ALL SELECT adopted_rev FROM playbook_standing_launches
             UNION ALL SELECT origin_rev FROM playbook_drafts",
        )
        .fetch_all(&pool)
        .await
        .expect("pins");
        assert_eq!(pins, vec![old; 3]);
        let origin: Option<String> =
            sqlx::query_scalar("SELECT origin_digest FROM playbook_drafts WHERE id = 'd'")
                .fetch_one(&pool)
                .await
                .expect("origin pin");
        assert_eq!(origin.as_deref(), Some(tree.as_str()));
        let revisions: Vec<(String, String)> =
            sqlx::query_as("SELECT playbook_id, tree_digest FROM playbook_revisions")
                .fetch_all(&pool)
                .await
                .expect("revisions");
        assert_eq!(revisions, vec![("p".to_string(), tree)]);
    }

    /// An older controller that rewrites the bytes of a row while it is being converted does not
    /// get a tree digest pinned to bytes it no longer holds.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_row_rewritten_after_it_was_read_is_not_pinned(pool: PgPool) {
        let first = pack(&[("a", b"1")]).tarball().expect("encode");
        let second = pack(&[("a", b"2")]).tarball().expect("encode");
        seed_launch_pack(&pool, "a", &second).await;

        let mut tx = pool.begin().await.expect("tx");
        let stale = put_tree(
            &mut tx,
            &EncodedPack::new(pack(&[("a", b"1")])).expect("encode"),
        )
        .await
        .expect("put");
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

    /// What an older controller left for a draft-sourced playbook converts in one pass: the rev
    /// becomes the tree digest, and the secret binding and the draft forked at the old rev are
    /// pinned to the tree, so the binding is not stale. A second pass, and a republish of the same
    /// content, change none of it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_draft_sourced_pin_survives_conversion_restarts_and_republish(pool: PgPool) {
        use crate::playbooks::drafts::{DraftSeed, SKELETON_MANIFEST, SKELETON_WORKFLOW};
        use crate::secrets::launch::{PackPin, Revision, Scope};
        let platform = crate::authz::model::Principal::platform();
        crate::playbooks::drafts::create(
            &pool,
            "studio",
            "d",
            DraftSeed::Skeleton,
            None,
            &platform,
        )
        .await
        .expect("draft");
        let tree = pack(&[
            ("crucible.toml", SKELETON_MANIFEST.as_bytes()),
            ("workflow.star", SKELETON_WORKFLOW.as_bytes()),
        ]);
        let publication = || crate::playbooks::registry::PublishDraft {
            id: "survey".to_string(),
            owner: platform.clone(),
            description: "published".to_string(),
            draft: "studio".to_string(),
            version: 1,
            tree: tree.clone(),
            replaces: false,
            accept_exposure_digest: None,
        };
        crate::playbooks::registry::publish_draft(&pool, publication(), None)
            .await
            .expect("publish");

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("crucible.toml"), SKELETON_MANIFEST).expect("write");
        std::fs::write(dir.path().join("workflow.star"), SKELETON_WORKFLOW).expect("write");
        let legacy = crate::playbooks::packs::tar_pack_tree(dir.path()).expect("tar");
        let old = content_digest(&legacy);
        sqlx::query(
            "UPDATE playbooks SET rev = $2, tar_gz = $1, tar_digest = $2, tree_digest = NULL
             WHERE id = 'survey'",
        )
        .bind(&legacy)
        .bind(&old)
        .execute(&pool)
        .await
        .expect("the row an older controller published");
        sqlx::query(
            "INSERT INTO playbook_drafts (id, description, origin_playbook, origin_rev, created_at,
                                          updated_at)
             VALUES ('fork', 'd', 'survey', $1, 'now', 'now')",
        )
        .bind(&old)
        .execute(&pool)
        .await
        .expect("a draft forked at the old rev");
        let owner = crate::authz::model::Principal::parse("user:alice").expect("owner");
        let name = crate::secrets::SecretName::parse("pr_token").expect("name");
        let mut conn = pool.acquire().await.expect("conn");
        crate::secrets::store::insert(
            &mut conn,
            &crate::secrets::store::NewSecret {
                id: "secret-1",
                name: &name,
                owner: &owner,
                kind: crate::secrets::SecretKind::Opaque,
                visibility: crate::secrets::Visibility::BrokerOnly,
                consumer: crate::secrets::ConsumerClass::Run,
                mode: crate::secrets::SecretMode::Managed,
                vault_path: "user:alice/pr",
                current_version: Some(1),
                created_by: Some("alice"),
            },
        )
        .await
        .expect("register");
        let bound = crate::secrets::store::insert_binding(
            &mut conn,
            &crate::secrets::store::NewBinding {
                id: "binding-1",
                secret_id: "secret-1",
                scope_kind: crate::secrets::ScopeKind::Playbook,
                scope_id: "survey",
                projection_kind: crate::secrets::ProjectionKind::Env,
                projection: "PR_TOKEN",
                declared_name: &name,
                pack_rev: Some(&old),
                schema_digest: None,
                created_by: Some("alice"),
            },
        )
        .await
        .expect("bind");
        assert_eq!(bound.pack_digest, None, "the row had no tree to pin");

        let launch_check = || async {
            let row = crate::playbooks::registry::get(&pool, "survey")
                .await
                .expect("read")
                .expect("row");
            let tree = row.tree_digest.as_ref().map(|t| t.to_string());
            crate::secrets::launch::resolve(
                &pool,
                &Scope::playbook("survey"),
                &[crate::secrets::manifest::DeclaredSecret {
                    name: name.clone(),
                    kind: crate::secrets::SecretKind::Opaque,
                    projection: None,
                }],
                &crate::authz::model::Principals::new(Some("alice"), &[]),
                Revision::Published(Some(PackPin::Registered {
                    rev: &row.rev,
                    tree: tree.as_deref(),
                })),
                None,
            )
            .await
            .expect("resolve")
            .map(|mints| mints.len())
        };
        let pins = || async {
            sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
                "SELECT p.rev, p.tree_digest, b.pack_digest, d.origin_digest
                 FROM playbooks p, secret_bindings b, playbook_drafts d
                 WHERE p.id = 'survey' AND d.id = 'fork'",
            )
            .fetch_one(&pool)
            .await
            .expect("pins")
        };

        let report = convert_pack_trees(&pool).await.expect("convert");
        assert_eq!(report.pinned, 3, "{report:?}");
        let digest = tree.digest().to_string();
        let converted = pins().await;
        assert_eq!(
            converted,
            (
                digest.clone(),
                Some(digest.clone()),
                Some(digest.clone()),
                Some(digest.clone())
            )
        );
        assert_eq!(launch_check().await, Ok(1));
        let fork = crate::playbooks::drafts::get(&pool, "fork")
            .await
            .expect("read")
            .expect("fork")
            .origin
            .expect("origin");
        assert!(!fork.moved(), "the old rev and the tree name the same pack");

        assert_eq!(
            convert_pack_trees(&pool).await.expect("restart"),
            ConversionReport::default()
        );
        assert_eq!(pins().await, converted);
        assert_eq!(launch_check().await, Ok(1));

        let republished = crate::playbooks::registry::publish_draft(
            &pool,
            crate::playbooks::registry::PublishDraft {
                replaces: true,
                ..publication()
            },
            None,
        )
        .await
        .expect("republish");
        assert_eq!(republished.rev, digest);
        assert_eq!(pins().await, converted);
        assert_eq!(launch_check().await, Ok(1));
        assert_eq!(
            convert_pack_trees(&pool).await.expect("restart"),
            ConversionReport::default()
        );
    }
}

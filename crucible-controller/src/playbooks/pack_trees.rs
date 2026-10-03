//! Pack trees stored once by `tree1:` digest (RFC-0002:C-PACK-BASE, ADR-0059): `pack_trees` holds
//! one row per tree and `pack_tree_files` its files.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result};
use crucible_contract::pack_tree::{PackTree, TreeDigest};
use sqlx::PgConnection;

/// Store `tree` under its digest unless it is already stored, and return the digest.
pub(crate) async fn put_tree(conn: &mut PgConnection, tree: &PackTree) -> Result<TreeDigest> {
    let (digest, file_hashes) = tree.digest_with_file_hashes();
    let stored: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pack_trees WHERE digest = $1")
        .bind(digest.as_str())
        .fetch_optional(&mut *conn)
        .await
        .context("looking up a pack tree")?;
    if stored.is_some() {
        return Ok(digest);
    }
    let delivered = tree.tarball().context("encoding the pack tarball")?.len();
    let total: usize = tree.files().values().map(Vec::len).sum();
    let inserted = sqlx::query(
        "INSERT INTO pack_trees (digest, file_count, total_bytes, delivered_bytes, created_at)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (digest) DO NOTHING",
    )
    .bind(digest.as_str())
    .bind(i32::try_from(tree.files().len()).context("pack file count")?)
    .bind(i64::try_from(total).context("pack size")?)
    .bind(i64::try_from(delivered).context("pack delivered size")?)
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

#[cfg(test)]
mod tests {
    use crate::playbooks::pack_trees::*;
    use sqlx::PgPool;

    fn pack(files: &[(&str, &[u8])]) -> PackTree {
        PackTree::from_pairs(files).expect("tree")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn storing_a_tree_twice_keeps_one_copy(pool: PgPool) {
        let tree = pack(&[("crucible.toml", b"m"), ("tools/run.sh", b"r")]);
        let mut conn = pool.acquire().await.expect("conn");

        let first = put_tree(&mut conn, &tree).await.expect("put");
        let second = put_tree(&mut conn, &tree).await.expect("put again");

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

    /// A controller that predates tree storage rewrites the bytes without the tree column; the
    /// trigger clears the column so conversion picks the row up again.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn rewriting_legacy_bytes_alone_clears_the_tree_digest(pool: PgPool) {
        let mut conn = pool.acquire().await.expect("conn");
        let digest = put_tree(&mut conn, &pack(&[("a", b"1")]))
            .await
            .expect("put");
        sqlx::query(
            "INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at, tree_digest)
             VALUES ('s', 'old', 'sha256:x', 1, 'now', $1)",
        )
        .bind(digest.as_str())
        .execute(&pool)
        .await
        .expect("seed");

        sqlx::query("UPDATE pack_tarballs SET tar_gz = 'new' WHERE issue_slug = 's'")
            .execute(&pool)
            .await
            .expect("old writer");
        let cleared: Option<String> =
            sqlx::query_scalar("SELECT tree_digest FROM pack_tarballs WHERE issue_slug = 's'")
                .fetch_one(&pool)
                .await
                .expect("read");
        assert_eq!(cleared, None);

        sqlx::query(
            "UPDATE pack_tarballs SET tar_gz = 'newer', tree_digest = $1 WHERE issue_slug = 's'",
        )
        .bind(digest.as_str())
        .execute(&pool)
        .await
        .expect("new writer");
        let kept: Option<String> =
            sqlx::query_scalar("SELECT tree_digest FROM pack_tarballs WHERE issue_slug = 's'")
                .fetch_one(&pool)
                .await
                .expect("read");
        assert_eq!(kept.as_deref(), Some(digest.as_str()));
    }
}

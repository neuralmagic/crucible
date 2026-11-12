//! What a stored pack tree's `crucible.lock` pins: `pack_tree_deps` and `pack_tree_tools` keep a
//! stored library tree or tool download for as long as a stored pack tree's lock names it.

use anyhow::Context as _;
use crucible::plan::deps::{LOCK_FILE, Lock};
use crucible_contract::pack_tree::PackTree;
use sqlx::PgConnection;

/// The lock a lock file's text holds. One that does not parse is no lock; the compile reports it.
pub(crate) fn parse_lock(lock: Option<&[u8]>) -> Option<Lock> {
    lock.and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| Lock::parse(text, std::path::Path::new(LOCK_FILE)).ok())
}

/// `tree`'s lock.
pub(crate) fn lock_of(tree: &PackTree) -> Option<Lock> {
    parse_lock(
        tree.files()
            .iter()
            .find(|(path, _)| path.as_str() == LOCK_FILE)
            .map(|(_, bytes)| bytes.as_slice()),
    )
}

/// Record which stored trees and downloads `tree`'s lock pins.
pub(crate) async fn pin(conn: &mut PgConnection, tree: &PackTree) -> anyhow::Result<()> {
    pin_deps(&mut *conn, tree).await?;
    pin_tools(conn, tree).await
}

/// Record which stored trees `tree`'s lock pins. A dependency that is not stored gets no row.
async fn pin_deps(conn: &mut PgConnection, tree: &PackTree) -> anyhow::Result<()> {
    let deps = lock_of(tree).map_or_else(Vec::new, |lock| lock.deps);
    if deps.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = deps.iter().map(|dep| dep.name.as_str()).collect();
    let digests: Vec<&str> = deps.iter().map(|dep| dep.tree.as_str()).collect();
    sqlx::query(
        "INSERT INTO pack_tree_deps (tree_digest, name, dep_digest)
         SELECT $1, d.name, d.digest FROM UNNEST($2::TEXT[], $3::TEXT[]) AS d(name, digest)
         JOIN pack_trees t ON t.digest = d.digest
         ON CONFLICT (tree_digest, name) DO NOTHING",
    )
    .bind(tree.digest().as_str())
    .bind(&names)
    .bind(&digests)
    .execute(conn)
    .await
    .context("pinning a pack's dependencies")?;
    Ok(())
}

/// Record which stored downloads `tree`'s lock pins. One that is not stored gets no row.
async fn pin_tools(conn: &mut PgConnection, tree: &PackTree) -> anyhow::Result<()> {
    let tools = lock_of(tree).map_or_else(Vec::new, |lock| lock.tools);
    if tools.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    let platforms: Vec<&str> = tools.iter().map(|tool| tool.platform.as_str()).collect();
    let digests: Vec<&str> = tools.iter().map(|tool| tool.sha256.as_str()).collect();
    sqlx::query(
        "INSERT INTO pack_tree_tools (tree_digest, name, platform, sha256)
         SELECT $1, t.name, t.platform, t.sha256
         FROM UNNEST($2::TEXT[], $3::TEXT[], $4::TEXT[]) AS t(name, platform, sha256)
         JOIN tool_blobs b ON b.sha256 = t.sha256
         ON CONFLICT (tree_digest, name, platform) DO NOTHING",
    )
    .bind(tree.digest().as_str())
    .bind(&names)
    .bind(&platforms)
    .bind(&digests)
    .execute(conn)
    .await
    .context("pinning a pack's tools")?;
    Ok(())
}

/// Delete every stored download created before `cutoff` that no stored pack tree pins.
pub(crate) async fn collect_tools(conn: &mut PgConnection, cutoff: &str) -> anyhow::Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM tool_blobs b WHERE b.created_at < $1
           AND NOT EXISTS (SELECT 1 FROM pack_tree_tools p WHERE p.sha256 = b.sha256)",
    )
    .bind(cutoff)
    .execute(conn)
    .await
    .context("deleting unpinned tools")?
    .rows_affected())
}

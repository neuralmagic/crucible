//! Preferences keyed by identity rather than by session. [`crate::identity::session`] holds scratch UI state
//! that lapses with the inactivity window; a document stored here comes back in a fresh browser.
//!
//! The document is a client-owned JSON blob: the server enforces the size cap on the route and
//! nothing about the shape, so a new setting needs no migration.

use anyhow::{Context, Result};
use sqlx::PgPool;

/// The `kind` the editor's settings live under.
pub const EDITOR_KIND: &str = "editor";
/// The `kind` the pickers' stars and filters live under.
pub const PICKERS_KIND: &str = "pickers";

/// One user's document of that kind, or `None` when they never saved one.
pub async fn get(pool: &PgPool, user: &str, kind: &str) -> Result<Option<serde_json::Value>> {
    let doc: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT doc FROM user_prefs WHERE user_id = $1 AND kind = $2")
            .bind(user)
            .bind(kind)
            .fetch_optional(pool)
            .await
            .context("reading a user prefs document")?;
    Ok(doc)
}

/// Store one user's document of that kind, replacing whatever was there.
pub async fn put(pool: &PgPool, user: &str, kind: &str, doc: &serde_json::Value) -> Result<()> {
    sqlx::query(
        "INSERT INTO user_prefs (user_id, kind, doc, updated_at) VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, kind) DO UPDATE SET doc = EXCLUDED.doc, updated_at = EXCLUDED.updated_at",
    )
    .bind(user)
    .bind(kind)
    .bind(doc)
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("storing a user prefs document")?;
    Ok(())
}

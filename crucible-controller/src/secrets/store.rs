//! The registry's Postgres half: secret metadata, bindings, and the audit trail.
//!
//! No function here reads or writes a value — the bytes live in Vault and reach this module only
//! as a version number. Every mutation takes a transaction, because a mutation and its audit row
//! land together or not at all, and the Vault write rides inside the same transaction so a failed
//! write rolls the row back instead of leaving a registration pointing at nothing.

use crate::authz::model::Principal;
use crate::secrets::{
    AuditAction, ConsumerClass, ProjectionKind, ScopeKind, SecretKind, SecretMode, SecretName,
    Visibility,
};
use sqlx::FromRow;

use anyhow::{Context, Result};
use sqlx::{PgConnection, Row, postgres::PgRow};

const SECRET_COLUMNS: &str = "id, name, owner, kind, visibility, consumer, mode, vault_path, \
                              current_version, created_by, created_at, updated_at";

const BINDING_COLUMNS: &str = "id, secret_id, scope_kind, scope_id, projection_kind, projection, \
                               declared_name, pack_rev, schema_digest, created_by, created_at";

/// Why a write was refused by the registry's own constraints.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A uniqueness constraint said the row is already there. `what` is the sentence the API hands
    /// back with a 409.
    #[error("{what}")]
    Duplicate { what: String },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// One registered secret. Metadata only: `vault_path` is where the bytes are, never what they are.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SecretRow {
    pub id: String,
    #[sqlx(try_from = "String")]
    pub name: SecretName,
    #[sqlx(try_from = "String")]
    pub owner: Principal,
    pub kind: SecretKind,
    pub visibility: Visibility,
    pub consumer: ConsumerClass,
    pub mode: SecretMode,
    /// A managed secret's path under the registry mount, or a reference's `vault://` URL.
    pub vault_path: String,
    pub current_version: Option<i64>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// What a registration stores. The value is not here: it went to Vault, and what came back is the
/// version.
#[derive(Debug, Clone)]
pub struct NewSecret<'a> {
    pub id: &'a str,
    pub name: &'a SecretName,
    pub owner: &'a Principal,
    pub kind: SecretKind,
    pub visibility: Visibility,
    pub consumer: ConsumerClass,
    pub mode: SecretMode,
    pub vault_path: &'a str,
    pub current_version: Option<i64>,
    pub created_by: Option<&'a str>,
}

/// One binding: a secret attached to a scope, under the declared name it satisfies.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BindingRow {
    pub id: String,
    pub secret_id: String,
    pub scope_kind: ScopeKind,
    pub scope_id: String,
    pub projection_kind: ProjectionKind,
    pub projection: String,
    #[sqlx(try_from = "String")]
    pub declared_name: SecretName,
    pub pack_rev: Option<String>,
    pub schema_digest: Option<String>,
    pub created_by: Option<String>,
    pub created_at: String,
}

/// What a bind stores.
#[derive(Debug, Clone)]
pub struct NewBinding<'a> {
    pub id: &'a str,
    pub secret_id: &'a str,
    pub scope_kind: ScopeKind,
    pub scope_id: &'a str,
    pub projection_kind: ProjectionKind,
    pub projection: &'a str,
    pub declared_name: &'a SecretName,
    pub pack_rev: Option<&'a str>,
    pub schema_digest: Option<&'a str>,
    pub created_by: Option<&'a str>,
}

/// One line of the trail.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AuditRow {
    pub id: i64,
    pub secret_id: Option<String>,
    pub secret_name: String,
    pub owner: String,
    pub action: AuditAction,
    /// The acting principal. `None` only for a hub read on the hub's own behalf.
    pub actor: Option<String>,
    pub detail: Option<String>,
    pub at: String,
}

/// Store a registration. The caller has already written Vault, or is about to inside the same
/// transaction.
pub async fn insert(conn: &mut PgConnection, new: &NewSecret<'_>) -> Result<SecretRow, StoreError> {
    let now = crate::clock::now_rfc3339();
    let row = sqlx::query(&format!(
        r#"INSERT INTO secrets (id, name, owner, kind, visibility, consumer, mode, vault_path,
                                current_version, created_by, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11)
           RETURNING {SECRET_COLUMNS}"#
    ))
    .bind(new.id)
    .bind(new.name.as_str())
    .bind(new.owner.to_string())
    .bind(new.kind.as_str())
    .bind(new.visibility.as_str())
    .bind(new.consumer.as_str())
    .bind(new.mode.as_str())
    .bind(new.vault_path)
    .bind(new.current_version)
    .bind(new.created_by)
    .bind(&now)
    .fetch_one(conn)
    .await;
    match row {
        Ok(row) => Ok(SecretRow::from_row(&row).context("reading back a stored secret")?),
        Err(e) if is_unique_violation(&e) => Err(StoreError::Duplicate {
            what: format!("{} already has a secret named {}", new.owner, new.name),
        }),
        Err(e) => Err(StoreError::Internal(
            anyhow::Error::new(e).context("storing a secret"),
        )),
    }
}

/// One secret by id.
pub async fn get(pool: &sqlx::PgPool, id: &str) -> Result<Option<SecretRow>> {
    sqlx::query_as::<_, SecretRow>(&format!(
        "SELECT {SECRET_COLUMNS} FROM secrets WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("reading a secret")
}

/// Every secret, or every secret owned by one of `owners`. Newest first.
pub async fn list(pool: &sqlx::PgPool, owners: Option<&[Principal]>) -> Result<Vec<SecretRow>> {
    let rows = match owners {
        None => sqlx::query_as::<_, SecretRow>(&format!(
            "SELECT {SECRET_COLUMNS} FROM secrets ORDER BY created_at DESC, id"
        ))
        .fetch_all(pool)
        .await
        .context("listing secrets")?,
        Some(owners) => {
            let owners: Vec<String> = owners.iter().map(Principal::to_string).collect();
            sqlx::query_as::<_, SecretRow>(&format!(
                "SELECT {SECRET_COLUMNS} FROM secrets WHERE owner = ANY($1)
                 ORDER BY created_at DESC, id"
            ))
            .bind(&owners)
            .fetch_all(pool)
            .await
            .context("listing secrets by owner")?
        }
    };
    Ok(rows)
}

/// Every secret registered under `name`. A name is unique per owner, not globally, so a caller that
/// has only a name (a provider registration does) gets back everything that answers to it and
/// decides what an ambiguous answer means.
pub async fn find_by_name(pool: &sqlx::PgPool, name: &SecretName) -> Result<Vec<SecretRow>> {
    sqlx::query_as::<_, SecretRow>(&format!(
        "SELECT {SECRET_COLUMNS} FROM secrets WHERE name = $1 ORDER BY owner"
    ))
    .bind(name.as_str())
    .fetch_all(pool)
    .await
    .context("reading a secret by name")
}

/// The one secret `owner` registered under `name`. Names are unique per owner, so this is the
/// whole reference: what another owner calls the same thing cannot change the answer.
pub async fn find_owned(
    pool: &sqlx::PgPool,
    owner: &Principal,
    name: &SecretName,
) -> Result<Option<SecretRow>> {
    sqlx::query_as::<_, SecretRow>(&format!(
        "SELECT {SECRET_COLUMNS} FROM secrets WHERE owner = $1 AND name = $2"
    ))
    .bind(owner.to_string())
    .bind(name.as_str())
    .fetch_optional(pool)
    .await
    .context("reading a secret by owner and name")
}

/// Advance the stored version after a rotation wrote a new one to Vault.
pub async fn set_version(conn: &mut PgConnection, id: &str, version: i64) -> Result<()> {
    sqlx::query("UPDATE secrets SET current_version = $2, updated_at = $3 WHERE id = $1")
        .bind(id)
        .bind(version)
        .bind(crate::clock::now_rfc3339())
        .execute(conn)
        .await
        .context("advancing a secret's version")?;
    Ok(())
}

/// Hand a secret to another principal. The bytes of a managed secret live under the owner's path,
/// so the caller moves them in Vault and passes the new path (and the version the copy landed at);
/// a reference or a minted secret keeps its path. A model provider names a secret by owner and
/// name, so every provider pointing at the old owner is re-pointed in the same statement batch.
pub async fn transfer(
    conn: &mut PgConnection,
    id: &str,
    from: &Principal,
    name: &SecretName,
    to: &Principal,
    vault_path: &str,
    current_version: Option<i64>,
) -> Result<SecretRow, StoreError> {
    let now = crate::clock::now_rfc3339();
    let row = sqlx::query(&format!(
        r#"UPDATE secrets SET owner = $2, vault_path = $3, current_version = $4, updated_at = $5
           WHERE id = $1
           RETURNING {SECRET_COLUMNS}"#
    ))
    .bind(id)
    .bind(to.to_string())
    .bind(vault_path)
    .bind(current_version)
    .bind(&now)
    .fetch_one(&mut *conn)
    .await;
    let row = match row {
        Ok(row) => SecretRow::from_row(&row).context("reading back a transferred secret")?,
        Err(e) if is_unique_violation(&e) => {
            return Err(StoreError::Duplicate {
                what: format!("{to} already has a secret named {name}"),
            });
        }
        Err(e) => {
            return Err(StoreError::Internal(
                anyhow::Error::new(e).context("transferring a secret"),
            ));
        }
    };
    sqlx::query(
        "UPDATE model_providers SET secret_owner = $3, updated_at = $4
         WHERE secret_name = $1 AND secret_owner = $2",
    )
    .bind(name.as_str())
    .bind(from.to_string())
    .bind(to.to_string())
    .bind(&now)
    .execute(conn)
    .await
    .context("re-pointing model providers at a transferred secret")?;
    Ok(row)
}

/// Remove the metadata row. The caller has checked that no binding references it.
pub async fn delete(conn: &mut PgConnection, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM secrets WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await
        .context("deleting a secret")?;
    Ok(())
}

/// Store a binding.
pub async fn insert_binding(
    conn: &mut PgConnection,
    new: &NewBinding<'_>,
) -> Result<BindingRow, StoreError> {
    let row = sqlx::query(&format!(
        r#"INSERT INTO secret_bindings (id, secret_id, scope_kind, scope_id, projection_kind,
                                        projection, declared_name, pack_rev, schema_digest,
                                        created_by, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
           RETURNING {BINDING_COLUMNS}"#
    ))
    .bind(new.id)
    .bind(new.secret_id)
    .bind(new.scope_kind.as_str())
    .bind(new.scope_id)
    .bind(new.projection_kind.as_str())
    .bind(new.projection)
    .bind(new.declared_name.as_str())
    .bind(new.pack_rev)
    .bind(new.schema_digest)
    .bind(new.created_by)
    .bind(crate::clock::now_rfc3339())
    .fetch_one(conn)
    .await;
    match row {
        Ok(row) => Ok(BindingRow::from_row(&row).context("reading back a stored binding")?),
        Err(e) if is_unique_violation(&e) => Err(StoreError::Duplicate {
            what: format!(
                "{} {} already binds {} (or something else as {} {})",
                new.scope_kind.as_str(),
                new.scope_id,
                new.declared_name,
                new.projection_kind.as_str(),
                new.projection
            ),
        }),
        Err(e) => Err(StoreError::Internal(
            anyhow::Error::new(e).context("storing a binding"),
        )),
    }
}

/// One binding by id, for the unbind path's ownership check.
pub async fn get_binding(pool: &sqlx::PgPool, id: &str) -> Result<Option<BindingRow>> {
    sqlx::query_as::<_, BindingRow>(&format!(
        "SELECT {BINDING_COLUMNS} FROM secret_bindings WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("reading a binding")
}

/// Remove a binding. Returns whether a row was there.
pub async fn delete_binding(conn: &mut PgConnection, id: &str) -> Result<bool> {
    let done = sqlx::query("DELETE FROM secret_bindings WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await
        .context("deleting a binding")?;
    Ok(done.rows_affected() > 0)
}

/// Every binding of one secret — what the delete check reads.
pub async fn bindings_for_secret(pool: &sqlx::PgPool, secret_id: &str) -> Result<Vec<BindingRow>> {
    sqlx::query_as::<_, BindingRow>(&format!(
        "SELECT {BINDING_COLUMNS} FROM secret_bindings WHERE secret_id = $1
         ORDER BY created_at, id"
    ))
    .bind(secret_id)
    .fetch_all(pool)
    .await
    .context("listing a secret's bindings")
}

/// Every binding on one scope, with the secret each resolves to — the launch check's read.
pub async fn bindings_for_scope(
    pool: &sqlx::PgPool,
    scope_kind: ScopeKind,
    scope_id: &str,
) -> Result<Vec<(BindingRow, SecretRow)>> {
    let binding_columns = BINDING_COLUMNS
        .split(", ")
        .map(|c| format!("b.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let secret_columns = SECRET_COLUMNS
        .split(", ")
        .map(|c| format!("s.{c} AS sec_{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = sqlx::query(&format!(
        "SELECT {binding_columns}, {secret_columns}
         FROM secret_bindings b JOIN secrets s ON s.id = b.secret_id
         WHERE b.scope_kind = $1 AND b.scope_id = $2
         ORDER BY b.declared_name"
    ))
    .bind(scope_kind.as_str())
    .bind(scope_id)
    .fetch_all(pool)
    .await
    .context("listing a scope's bindings")?;
    rows.iter()
        .map(|row| {
            let binding = BindingRow::from_row(row)?;
            let secret = secret_from_prefixed(row)?;
            Ok((binding, secret))
        })
        .collect()
}

/// The joined half of [`bindings_for_scope`]: the secret columns come back under a `sec_` prefix,
/// so neither `id` nor the binding's own `secret_id` collides with them.
fn secret_from_prefixed(row: &PgRow) -> Result<SecretRow> {
    let owner: String = row.try_get("sec_owner")?;
    let name: String = row.try_get("sec_name")?;
    let kind: String = row.try_get("sec_kind")?;
    let visibility: String = row.try_get("sec_visibility")?;
    let consumer: String = row.try_get("sec_consumer")?;
    let mode: String = row.try_get("sec_mode")?;
    Ok(SecretRow {
        id: row.try_get("sec_id")?,
        name: SecretName::parse(&name).context("a stored secret name")?,
        owner: Principal::parse(&owner).context("a stored owner principal")?,
        kind: SecretKind::parse(&kind)?,
        visibility: Visibility::parse(&visibility)?,
        consumer: ConsumerClass::parse(&consumer)?,
        mode: SecretMode::parse(&mode)?,
        vault_path: row.try_get("sec_vault_path")?,
        current_version: row.try_get("sec_current_version")?,
        created_by: row.try_get("sec_created_by")?,
        created_at: row.try_get("sec_created_at")?,
        updated_at: row.try_get("sec_updated_at")?,
    })
}

/// One audit line, written in the same transaction as the action it records.
#[derive(Debug, Clone)]
pub struct NewAudit<'a> {
    pub secret_id: Option<&'a str>,
    pub secret_name: &'a str,
    pub owner: &'a Principal,
    pub action: AuditAction,
    /// The acting principal. `None` only where there is none.
    pub actor: Option<&'a Principal>,
    pub detail: Option<&'a str>,
}

/// Append one audit row.
pub async fn audit(conn: &mut PgConnection, entry: &NewAudit<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO secret_audit (secret_id, secret_name, owner, action, actor, detail, at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(entry.secret_id)
    .bind(entry.secret_name)
    .bind(entry.owner.to_string())
    .bind(entry.action.as_str())
    .bind(entry.actor.map(Principal::to_string))
    .bind(entry.detail)
    .bind(crate::clock::now_rfc3339())
    .execute(conn)
    .await
    .context("appending a secret audit row")?;
    Ok(())
}

/// The trail of one secret, newest first.
pub async fn audit_for_secret(
    pool: &sqlx::PgPool,
    secret_id: &str,
    limit: i64,
) -> Result<Vec<AuditRow>> {
    sqlx::query_as::<_, AuditRow>(
        "SELECT id, secret_id, secret_name, owner, action, actor, detail, at
         FROM secret_audit WHERE secret_id = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(secret_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("reading a secret's audit trail")
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

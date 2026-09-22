//! The Postgres half of teams and the authorization audit trail.
//!
//! Every mutation takes a transaction, because a mutation and its audit row land together or not
//! at all (RFC-0003 C-AUDIT: an operation whose audit record cannot be persisted fails).

use crate::authz::model::{Member, MemberKind, MemberRef, Principal, TeamRole, TeamSlug};
use anyhow::{Context, Result};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgConnection, PgExecutor, Row};

/// Why a write was refused by the store's own constraints.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A uniqueness constraint said the row is already there. `what` is the sentence the API hands
    /// back with a 409.
    #[error("{what}")]
    Duplicate { what: String },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// One team.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TeamRow {
    #[sqlx(try_from = "String")]
    pub slug: TeamSlug,
    pub display_name: String,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// One member row, its kind and reference already parsed into a [`MemberRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    pub team: TeamSlug,
    pub member: MemberRef,
    pub role: TeamRole,
    pub since: String,
    pub added_by: Option<String>,
}

/// A member row as stored, before its reference is parsed.
#[derive(sqlx::FromRow)]
struct RawMemberRow {
    team: String,
    member_kind: MemberKind,
    member_ref: String,
    role: TeamRole,
    since: String,
    added_by: Option<String>,
}

/// Parse the rows that still parse. A row written under a spelling the running build no longer
/// accepts is skipped with a warning instead of failing every membership read.
fn decode_members(rows: Vec<RawMemberRow>) -> Vec<MemberRow> {
    rows.into_iter()
        .filter_map(|raw| {
            let team = match TeamSlug::parse(&raw.team) {
                Ok(team) => team,
                Err(e) => {
                    tracing::warn!(team = raw.team, error = %e, "team member row skipped");
                    return None;
                }
            };
            let member = match MemberRef::parse(raw.member_kind, &raw.member_ref) {
                Ok(member) => member,
                Err(e) => {
                    tracing::warn!(
                        team = raw.team,
                        kind = raw.member_kind.as_str(),
                        member = raw.member_ref,
                        error = %e,
                        "team member row skipped"
                    );
                    return None;
                }
            };
            Some(MemberRow {
                team,
                member,
                role: raw.role,
                since: raw.since,
                added_by: raw.added_by,
            })
        })
        .collect()
}

/// A signed-in user as the `users` table recorded them, for the reachability check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownUser {
    pub login: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

/// One governed resource a team owns, named in a delete refusal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct OwnedResource {
    pub resource_type: String,
    pub id: String,
    pub name: String,
}

/// One authorization audit record (RFC-0003 C-AUDIT).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub actor: String,
    pub subject: Option<String>,
    pub auth_path: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: String,
    pub allowed: bool,
    pub rule: String,
    pub prior: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
}

/// One line of the trail, as read back.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AuditRow {
    pub id: i64,
    pub at: String,
    pub actor: String,
    pub subject: Option<String>,
    pub auth_path: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: String,
    pub decision: String,
    pub rule: String,
    pub prior: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
}

const TEAM_COLUMNS: &str = "slug, display_name, created_by, created_at, updated_at";
const MEMBER_COLUMNS: &str = "team, member_kind, member_ref, role, since, added_by";
const AUDIT_COLUMNS: &str = "id, at, actor, subject, auth_path, action, resource_type, \
                             resource_id, decision, rule, prior, result";

pub async fn list_teams(ex: impl PgExecutor<'_>) -> Result<Vec<TeamRow>> {
    sqlx::query_as::<_, TeamRow>(&format!("SELECT {TEAM_COLUMNS} FROM teams ORDER BY slug"))
        .fetch_all(ex)
        .await
        .context("listing teams")
}

pub async fn get_team(ex: impl PgExecutor<'_>, slug: &TeamSlug) -> Result<Option<TeamRow>> {
    sqlx::query_as::<_, TeamRow>(&format!("SELECT {TEAM_COLUMNS} FROM teams WHERE slug = $1"))
        .bind(slug.as_str())
        .fetch_optional(ex)
        .await
        .context("reading a team")
}

/// Every member row of every team, which is what resolution and the cycle check walk.
pub async fn all_members(ex: impl PgExecutor<'_>) -> Result<Vec<MemberRow>> {
    let rows = sqlx::query_as::<_, RawMemberRow>(&format!(
        "SELECT {MEMBER_COLUMNS} FROM team_members ORDER BY team, member_kind, member_ref"
    ))
    .fetch_all(ex)
    .await
    .context("listing team members")?;
    Ok(decode_members(rows))
}

pub async fn members_of(ex: impl PgExecutor<'_>, slug: &TeamSlug) -> Result<Vec<MemberRow>> {
    let rows = sqlx::query_as::<_, RawMemberRow>(&format!(
        "SELECT {MEMBER_COLUMNS} FROM team_members WHERE team = $1 \
         ORDER BY member_kind, member_ref"
    ))
    .bind(slug.as_str())
    .fetch_all(ex)
    .await
    .context("listing a team's members")?;
    Ok(decode_members(rows))
}

pub async fn insert_team(
    conn: &mut PgConnection,
    slug: &TeamSlug,
    display_name: &str,
    created_by: Option<&Principal>,
    now: &str,
) -> Result<TeamRow, StoreError> {
    sqlx::query_as::<_, TeamRow>(&format!(
        "INSERT INTO teams (slug, display_name, created_by, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $4) RETURNING {TEAM_COLUMNS}"
    ))
    .bind(slug.as_str())
    .bind(display_name)
    .bind(created_by.map(Principal::to_string))
    .bind(now)
    .fetch_one(conn)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => StoreError::Duplicate {
            what: format!("a team with slug {slug} already exists"),
        },
        _ => StoreError::Internal(anyhow::Error::new(e).context("inserting a team")),
    })
}

pub async fn rename_team(
    conn: &mut PgConnection,
    slug: &TeamSlug,
    display_name: &str,
    now: &str,
) -> Result<Option<TeamRow>> {
    sqlx::query_as::<_, TeamRow>(&format!(
        "UPDATE teams SET display_name = $2, updated_at = $3 WHERE slug = $1 \
         RETURNING {TEAM_COLUMNS}"
    ))
    .bind(slug.as_str())
    .bind(display_name)
    .bind(now)
    .fetch_optional(conn)
    .await
    .context("renaming a team")
}

/// Delete a team and, by cascade, its member rows and its membership of other teams.
pub async fn delete_team(conn: &mut PgConnection, slug: &TeamSlug) -> Result<bool> {
    sqlx::query("DELETE FROM team_members WHERE member_kind = 'team' AND member_ref = $1")
        .bind(slug.as_str())
        .execute(&mut *conn)
        .await
        .context("removing a team from the teams it was a member of")?;
    let done = sqlx::query("DELETE FROM teams WHERE slug = $1")
        .bind(slug.as_str())
        .execute(conn)
        .await
        .context("deleting a team")?;
    Ok(done.rows_affected() == 1)
}

/// Replace a team's member set. The caller has already checked the invariants (an owner that
/// resolves to a user, no cycle); this is the write.
pub async fn replace_members(
    conn: &mut PgConnection,
    slug: &TeamSlug,
    members: &[Member],
    added_by: Option<&Principal>,
    now: &str,
) -> Result<Vec<MemberRow>> {
    sqlx::query("DELETE FROM team_members WHERE team = $1")
        .bind(slug.as_str())
        .execute(&mut *conn)
        .await
        .context("clearing a team's members")?;
    for m in members {
        sqlx::query(
            "INSERT INTO team_members (team, member_kind, member_ref, role, since, added_by) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (team, member_kind, member_ref) DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(slug.as_str())
        .bind(m.member.kind())
        .bind(m.member.stored())
        .bind(m.role)
        .bind(now)
        .bind(added_by.map(Principal::to_string))
        .execute(&mut *conn)
        .await
        .context("inserting a team member")?;
    }
    sqlx::query("UPDATE teams SET updated_at = $2 WHERE slug = $1")
        .bind(slug.as_str())
        .bind(now)
        .execute(&mut *conn)
        .await
        .context("stamping a team's update")?;
    members_of(conn, slug).await
}

/// Add members that are not already listed, leaving existing rows as they are. The startup seed's
/// write: it must never demote or remove what an administrator set by hand.
pub async fn add_members_if_absent(
    conn: &mut PgConnection,
    slug: &TeamSlug,
    members: &[Member],
    now: &str,
) -> Result<u64> {
    let mut added = 0;
    for m in members {
        let done = sqlx::query(
            "INSERT INTO team_members (team, member_kind, member_ref, role, since, added_by) \
             VALUES ($1, $2, $3, $4, $5, NULL) \
             ON CONFLICT (team, member_kind, member_ref) DO NOTHING",
        )
        .bind(slug.as_str())
        .bind(m.member.kind())
        .bind(m.member.stored())
        .bind(m.role)
        .bind(now)
        .execute(&mut *conn)
        .await
        .context("seeding a team member")?;
        added += done.rows_affected();
    }
    Ok(added)
}

/// Every signed-in user with the groups their last login stamped.
pub async fn known_users(ex: impl PgExecutor<'_>) -> Result<Vec<KnownUser>> {
    let rows = sqlx::query("SELECT login, email, groups FROM users")
        .fetch_all(ex)
        .await
        .context("listing known users")?;
    rows.into_iter()
        .map(|row| {
            let groups: Option<serde_json::Value> = row.try_get("groups")?;
            let groups = groups
                .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                .unwrap_or_default();
            Ok(KnownUser {
                login: row.try_get("login")?,
                email: row.try_get("email")?,
                groups,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .context("decoding known users")
}

/// The email a login's last sign-in carried, for the email-domain rule.
pub async fn email_of(ex: impl PgExecutor<'_>, login: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT email FROM users WHERE login = $1")
        .bind(login)
        .fetch_optional(ex)
        .await
        .context("reading a login's email")?;
    Ok(row.and_then(|r| r.try_get::<Option<String>, _>("email").ok().flatten()))
}

/// Every governed resource `owner` owns, across the tables that record an owner today. A team is
/// not deletable while this is non-empty.
pub async fn owned_by(ex: impl PgExecutor<'_>, owner: &Principal) -> Result<Vec<OwnedResource>> {
    let owner = owner.to_string();
    sqlx::query_as::<_, OwnedResource>(
        "SELECT 'secret' AS resource_type, id, name FROM secrets WHERE owner = $1
         UNION ALL
         SELECT 'standing_launch', id, playbook FROM playbook_standing_launches
          WHERE owner_principal = $1
         ORDER BY 1, 2",
    )
    .bind(owner)
    .fetch_all(ex)
    .await
    .context("listing a principal's resources")
}

impl<'r> FromRow<'r, PgRow> for OwnedResource {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        Ok(OwnedResource {
            resource_type: row.try_get("resource_type")?,
            id: row.try_get("id")?,
            name: row.try_get("name")?,
        })
    }
}

/// Append one audit record on the caller's connection, so it commits with the change it records.
pub async fn audit(conn: &mut PgConnection, event: &AuditEvent, now: &str) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO authz_audit (at, actor, subject, auth_path, action, resource_type, \
                                  resource_id, decision, rule, prior, result) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
    )
    .bind(now)
    .bind(&event.actor)
    .bind(&event.subject)
    .bind(&event.auth_path)
    .bind(&event.action)
    .bind(&event.resource_type)
    .bind(&event.resource_id)
    .bind(if event.allowed { "allow" } else { "deny" })
    .bind(&event.rule)
    .bind(&event.prior)
    .bind(&event.result)
    .fetch_one(conn)
    .await
    .context("writing the authorization audit record")?;
    Ok(id)
}

/// The trail for one resource, newest first.
pub async fn audit_for(
    ex: impl PgExecutor<'_>,
    resource_type: &str,
    resource_id: &str,
    limit: i64,
) -> Result<Vec<AuditRow>> {
    sqlx::query_as::<_, AuditRow>(&format!(
        "SELECT {AUDIT_COLUMNS} FROM authz_audit \
         WHERE resource_type = $1 AND resource_id = $2 ORDER BY id DESC LIMIT $3"
    ))
    .bind(resource_type)
    .bind(resource_id)
    .bind(limit)
    .fetch_all(ex)
    .await
    .context("reading the authorization audit trail")
}

/// One stored policy set.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PolicySetRow {
    pub digest: String,
    pub text: String,
    pub owner: String,
    pub schema_version: i32,
    pub created_by: Option<String>,
    pub created_at: String,
    pub activated_by: Option<String>,
    pub activated_at: Option<String>,
    pub active: bool,
}

const POLICY_COLUMNS: &str = "digest, text, owner, schema_version, created_by, created_at, \
                              activated_by, activated_at, active";

/// Store a validated set; the same text stored twice is one row.
pub async fn insert_policy_set(
    conn: &mut PgConnection,
    digest: &str,
    text: &str,
    schema_version: i32,
    created_by: Option<&str>,
    now: &str,
) -> Result<PolicySetRow> {
    sqlx::query(
        "INSERT INTO policy_sets (digest, text, owner, schema_version, created_by, created_at) \
         VALUES ($1, $2, $6, $3, $4, $5) ON CONFLICT (digest) DO NOTHING",
    )
    .bind(digest)
    .bind(text)
    .bind(schema_version)
    .bind(created_by)
    .bind(now)
    .bind(Principal::platform().to_string())
    .execute(&mut *conn)
    .await
    .context("storing a policy set")?;
    get_policy_set(conn, digest)
        .await?
        .context("the policy set just stored is missing")
}

pub async fn list_policy_sets(ex: impl PgExecutor<'_>) -> Result<Vec<PolicySetRow>> {
    sqlx::query_as::<_, PolicySetRow>(&format!(
        "SELECT {POLICY_COLUMNS} FROM policy_sets ORDER BY active DESC, created_at DESC"
    ))
    .fetch_all(ex)
    .await
    .context("listing policy sets")
}

pub async fn get_policy_set(ex: impl PgExecutor<'_>, digest: &str) -> Result<Option<PolicySetRow>> {
    sqlx::query_as::<_, PolicySetRow>(&format!(
        "SELECT {POLICY_COLUMNS} FROM policy_sets WHERE digest = $1"
    ))
    .bind(digest)
    .fetch_optional(ex)
    .await
    .context("reading a policy set")
}

pub async fn active_policy_set(ex: impl PgExecutor<'_>) -> Result<Option<PolicySetRow>> {
    sqlx::query_as::<_, PolicySetRow>(&format!(
        "SELECT {POLICY_COLUMNS} FROM policy_sets WHERE active"
    ))
    .fetch_optional(ex)
    .await
    .context("reading the active policy set")
}

/// Make `digest` the active set. Returns the previously active digest, or `None` when the set
/// does not exist.
pub async fn activate_policy_set(
    conn: &mut PgConnection,
    digest: &str,
    by: Option<&str>,
    now: &str,
) -> Result<Option<Option<String>>> {
    if get_policy_set(&mut *conn, digest).await?.is_none() {
        return Ok(None);
    }
    let prior: Option<String> =
        sqlx::query_scalar("UPDATE policy_sets SET active = FALSE WHERE active RETURNING digest")
            .fetch_optional(&mut *conn)
            .await
            .context("clearing the active policy set")?;
    sqlx::query(
        "UPDATE policy_sets SET active = TRUE, activated_by = $2, activated_at = $3 WHERE digest = $1",
    )
    .bind(digest)
    .bind(by)
    .bind(now)
    .execute(&mut *conn)
    .await
    .context("activating a policy set")?;
    Ok(Some(prior))
}

/// One share on a resource.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ShareRow {
    pub resource_type: String,
    pub resource_id: String,
    pub grantee: String,
    pub role: String,
    pub not_after: Option<String>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

const SHARE_COLUMNS: &str =
    "resource_type, resource_id, grantee, role, not_after, created_by, created_at, updated_at";

/// Every share on one resource, grantee order.
pub async fn shares_of(
    ex: impl PgExecutor<'_>,
    resource_type: &str,
    resource_id: &str,
) -> Result<Vec<ShareRow>> {
    sqlx::query_as::<_, ShareRow>(&format!(
        "SELECT {SHARE_COLUMNS} FROM resource_shares \
         WHERE resource_type = $1 AND resource_id = $2 ORDER BY grantee"
    ))
    .bind(resource_type)
    .bind(resource_id)
    .fetch_all(ex)
    .await
    .context("listing a resource's shares")
}

/// Every share of one type granted to any of `grantees`.
pub async fn shares_granted_to(
    ex: impl PgExecutor<'_>,
    resource_type: &str,
    grantees: &[String],
) -> Result<Vec<ShareRow>> {
    if grantees.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, ShareRow>(&format!(
        "SELECT {SHARE_COLUMNS} FROM resource_shares \
         WHERE resource_type = $1 AND grantee = ANY($2) ORDER BY resource_id, grantee"
    ))
    .bind(resource_type)
    .bind(grantees)
    .fetch_all(ex)
    .await
    .context("listing the shares a principal set holds")
}

/// Grant or change a share; the prior row, when one existed.
pub async fn upsert_share(conn: &mut PgConnection, share: &ShareRow) -> Result<Option<ShareRow>> {
    let prior = sqlx::query_as::<_, ShareRow>(&format!(
        "SELECT {SHARE_COLUMNS} FROM resource_shares \
         WHERE resource_type = $1 AND resource_id = $2 AND grantee = $3"
    ))
    .bind(&share.resource_type)
    .bind(&share.resource_id)
    .bind(&share.grantee)
    .fetch_optional(&mut *conn)
    .await
    .context("reading the prior share")?;
    sqlx::query(
        "INSERT INTO resource_shares (resource_type, resource_id, grantee, role, not_after, \
                                      created_by, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7) \
         ON CONFLICT (resource_type, resource_id, grantee) \
         DO UPDATE SET role = EXCLUDED.role, not_after = EXCLUDED.not_after, \
                       updated_at = EXCLUDED.updated_at",
    )
    .bind(&share.resource_type)
    .bind(&share.resource_id)
    .bind(&share.grantee)
    .bind(&share.role)
    .bind(&share.not_after)
    .bind(&share.created_by)
    .bind(&share.updated_at)
    .execute(&mut *conn)
    .await
    .context("writing the share")?;
    Ok(prior)
}

/// Revoke a share; the row it was, when one existed.
pub async fn delete_share(
    conn: &mut PgConnection,
    resource_type: &str,
    resource_id: &str,
    grantee: &str,
) -> Result<Option<ShareRow>> {
    sqlx::query_as::<_, ShareRow>(&format!(
        "DELETE FROM resource_shares \
         WHERE resource_type = $1 AND resource_id = $2 AND grantee = $3 RETURNING {SHARE_COLUMNS}"
    ))
    .bind(resource_type)
    .bind(resource_id)
    .bind(grantee)
    .fetch_optional(conn)
    .await
    .context("revoking the share")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::model::MemberKind;
    use sqlx::PgPool;

    fn slug(s: &str) -> TeamSlug {
        TeamSlug::parse(s).expect("slug")
    }

    fn member(kind: MemberKind, raw: &str, role: TeamRole) -> Member {
        Member {
            member: MemberRef::parse(kind, raw).expect("member"),
            role,
        }
    }

    /// A row a newer or older build wrote under a spelling this one refuses is skipped, not fatal:
    /// one such row must not take every membership read down with it.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_member_row_that_no_longer_parses_is_skipped(pool: PgPool) {
        let mut conn = pool.acquire().await.expect("conn");
        let now = crate::clock::now_rfc3339();
        insert_team(&mut conn, &slug("llm-d"), "LLM-D", None, &now)
            .await
            .expect("team");
        replace_members(
            &mut conn,
            &slug("llm-d"),
            &[member(MemberKind::User, "alice", TeamRole::Owner)],
            None,
            &now,
        )
        .await
        .expect("members");
        for (kind, raw) in [
            ("user", "not a login"),
            ("rule", "group-suffix:"),
            ("team", "x"),
        ] {
            sqlx::query(
                "INSERT INTO team_members (team, member_kind, member_ref, role, since) \
                 VALUES ('llm-d', $1, $2, 'member', $3)",
            )
            .bind(kind)
            .bind(raw)
            .bind(&now)
            .execute(&mut *conn)
            .await
            .expect("raw insert");
        }
        let expected = vec![MemberRef::User("alice".to_string())];
        let all: Vec<MemberRef> = all_members(&pool)
            .await
            .expect("all")
            .into_iter()
            .map(|m| m.member)
            .collect();
        assert_eq!(all, expected);
        let of: Vec<MemberRef> = members_of(&pool, &slug("llm-d"))
            .await
            .expect("of")
            .into_iter()
            .map(|m| m.member)
            .collect();
        assert_eq!(of, expected);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn teams_and_members_round_trip(pool: PgPool) {
        let mut conn = pool.acquire().await.expect("conn");
        let now = crate::clock::now_rfc3339();
        let alice = Principal::parse("user:alice").expect("principal");
        let team = insert_team(&mut conn, &slug("llm-d"), "LLM-D", Some(&alice), &now)
            .await
            .expect("insert");
        assert_eq!(team.created_by.as_deref(), Some("user:alice"));
        let dup = insert_team(&mut conn, &slug("llm-d"), "again", None, &now).await;
        assert!(matches!(dup, Err(StoreError::Duplicate { .. })), "{dup:?}");

        let rows = replace_members(
            &mut conn,
            &slug("llm-d"),
            &[
                member(MemberKind::User, "alice", TeamRole::Owner),
                member(MemberKind::Group, "/groups/llm-d", TeamRole::Member),
                member(
                    MemberKind::Rule,
                    "email-domain:example.com",
                    TeamRole::Member,
                ),
            ],
            Some(&alice),
            &now,
        )
        .await
        .expect("members");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].member, MemberRef::Group("/groups/llm-d".into()));
        assert_eq!(rows[2].role, TeamRole::Owner);

        insert_team(&mut conn, &slug("platform"), "Platform", None, &now)
            .await
            .expect("insert");
        replace_members(
            &mut conn,
            &slug("platform"),
            &[member(MemberKind::Team, "llm-d", TeamRole::Maintainer)],
            None,
            &now,
        )
        .await
        .expect("members");
        assert_eq!(all_members(&mut *conn).await.expect("all").len(), 4);

        let renamed = rename_team(&mut conn, &slug("llm-d"), "LLM-D core", &now)
            .await
            .expect("rename")
            .expect("exists");
        assert_eq!(renamed.display_name, "LLM-D core");
        assert!(
            rename_team(&mut conn, &slug("nope"), "x", &now)
                .await
                .expect("rename")
                .is_none()
        );

        assert!(
            delete_team(&mut conn, &slug("llm-d"))
                .await
                .expect("delete")
        );
        assert!(
            !delete_team(&mut conn, &slug("llm-d"))
                .await
                .expect("delete")
        );
        let left = all_members(&mut *conn).await.expect("all");
        assert!(
            left.is_empty(),
            "cascade and the nested edge both went: {left:?}"
        );
        assert_eq!(list_teams(&mut *conn).await.expect("list").len(), 1);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_seed_never_overwrites_a_hand_set_role(pool: PgPool) {
        let mut tx = pool.begin().await.expect("tx");
        let now = crate::clock::now_rfc3339();
        insert_team(&mut tx, &slug("admins"), "Admins", None, &now)
            .await
            .expect("insert");
        replace_members(
            &mut tx,
            &slug("admins"),
            &[member(MemberKind::User, "alice", TeamRole::Member)],
            None,
            &now,
        )
        .await
        .expect("members");
        let added = add_members_if_absent(
            &mut tx,
            &slug("admins"),
            &[
                member(MemberKind::User, "alice", TeamRole::Owner),
                member(MemberKind::User, "bob", TeamRole::Owner),
            ],
            &now,
        )
        .await
        .expect("seed");
        assert_eq!(added, 1);
        let rows = members_of(&mut *tx, &slug("admins"))
            .await
            .expect("members");
        assert_eq!(
            rows[0].role,
            TeamRole::Member,
            "alice kept her hand-set role"
        );
        assert_eq!(rows[1].role, TeamRole::Owner);
        tx.rollback().await.expect("rollback");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn audit_rows_read_back_newest_first(pool: PgPool) {
        let mut tx = pool.begin().await.expect("tx");
        let now = crate::clock::now_rfc3339();
        for rule in ["team-owner-all", "no-rule"] {
            audit(
                &mut tx,
                &AuditEvent {
                    actor: "user:alice".into(),
                    subject: None,
                    auth_path: "session".into(),
                    action: "team:update".into(),
                    resource_type: "team".into(),
                    resource_id: "llm-d".into(),
                    allowed: rule != "no-rule",
                    rule: rule.into(),
                    prior: Some(serde_json::json!({"display_name": "a"})),
                    result: None,
                },
                &now,
            )
            .await
            .expect("audit");
        }
        let rows = audit_for(&mut *tx, "team", "llm-d", 10)
            .await
            .expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].decision, "deny");
        assert_eq!(rows[1].decision, "allow");
        assert_eq!(
            rows[1].prior,
            Some(serde_json::json!({"display_name": "a"}))
        );
        assert!(
            audit_for(&mut *tx, "team", "other", 10)
                .await
                .expect("rows")
                .is_empty()
        );
        tx.rollback().await.expect("rollback");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn owned_by_reads_secrets_and_standing_launches(pool: PgPool) {
        let team = Principal::parse("team:llm-d").expect("principal");
        assert!(owned_by(&pool, &team).await.expect("owned").is_empty());
        sqlx::query(
            "INSERT INTO secrets (id, name, owner, kind, visibility, consumer, mode, vault_path, \
                                  created_at, updated_at) \
             VALUES ('s1', 'registry', 'team:llm-d', 'opaque', 'broker_only', 'run', 'managed', \
                     'p', 'now', 'now')",
        )
        .execute(&pool)
        .await
        .expect("secret");
        let owned = owned_by(&pool, &team).await.expect("owned");
        assert_eq!(
            owned,
            vec![OwnedResource {
                resource_type: "secret".into(),
                id: "s1".into(),
                name: "registry".into()
            }]
        );
        assert!(known_users(&pool).await.expect("users").is_empty());
        assert_eq!(email_of(&pool, "alice").await.expect("email"), None);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn policy_sets_store_once_and_activate_one_at_a_time(pool: PgPool) {
        let mut conn = pool.acquire().await.expect("conn");
        let now = crate::clock::now_rfc3339();
        let a = insert_policy_set(&mut conn, "d-a", "permit a", 1, None, &now)
            .await
            .expect("a");
        assert!(!a.active);
        let again = insert_policy_set(&mut conn, "d-a", "permit a", 1, Some("user:x"), &now)
            .await
            .expect("again");
        assert_eq!(again.created_by, None, "the first row stands");
        insert_policy_set(&mut conn, "d-b", "permit b", 1, None, &now)
            .await
            .expect("b");
        assert!(
            active_policy_set(&mut *conn)
                .await
                .expect("active")
                .is_none()
        );
        assert_eq!(
            activate_policy_set(&mut conn, "d-a", Some("user:root"), &now)
                .await
                .expect("activate"),
            Some(None)
        );
        assert_eq!(
            activate_policy_set(&mut conn, "d-b", Some("user:root"), &now)
                .await
                .expect("activate"),
            Some(Some("d-a".to_string()))
        );
        assert_eq!(
            activate_policy_set(&mut conn, "d-missing", None, &now)
                .await
                .expect("activate"),
            None
        );
        let active = active_policy_set(&mut *conn)
            .await
            .expect("active")
            .expect("one");
        assert_eq!(active.digest, "d-b");
        assert_eq!(active.activated_by.as_deref(), Some("user:root"));
        let listed = list_policy_sets(&mut *conn).await.expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].digest, "d-b", "active first");
    }
}

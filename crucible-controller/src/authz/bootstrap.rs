//! The configuration-only seed of the platform administrators team (RFC-0003 C-COMPATIBILITY):
//! a fresh install, or one whose administrators team reaches nobody, takes its owners from
//! `CONTROLLER_ADMINS` at startup. No request path creates the first administrator.

#![allow(clippy::disallowed_macros)]

use crate::authz::model::{Member, MemberRef, MembershipRule, Principal, TeamRole, TeamSlug};
use crate::authz::policy::{self, ActivePolicy, Engine, SCHEMA_VERSION};
use crate::authz::resolve;
use crate::authz::store::{self, AuditEvent};
use anyhow::{Context, Result};
use sqlx::PgPool;

/// What the seed did, for the startup log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedOutcome {
    /// The team already reaches a user; configuration was not consulted.
    Reachable,
    /// The team was created or refilled with this many owners from configuration.
    Seeded { added: u64 },
    /// The team reaches nobody and configuration names nobody either.
    Unreachable,
}

pub async fn seed_platform_administrators(pool: &PgPool, admins: &[String]) -> Result<SeedOutcome> {
    seed_team(
        pool,
        &TeamSlug::platform_administrators(),
        "Platform administrators",
        || {
            admins
                .iter()
                .filter_map(|login| match Principal::user(login) {
                    Ok(Principal::User(login)) => Some(Member {
                        member: MemberRef::User(login),
                        role: TeamRole::Owner,
                    }),
                    _ => {
                        tracing::warn!(
                            login,
                            "CONTROLLER_ADMINS entry is not a valid login; skipped"
                        );
                        None
                    }
                })
                .collect()
        },
    )
    .await
}

/// Seed the platform operators team from `CONTROLLER_OPERATORS` and `CONTROLLER_OPERATOR_GROUPS`
/// while it reaches nobody: logins as members, configured groups as `group-suffix` rules, which
/// is the tail match the operator guard applies.
pub async fn seed_platform_operators(
    pool: &PgPool,
    operators: &[String],
    operator_groups: &[String],
) -> Result<SeedOutcome> {
    seed_team(pool, &TeamSlug::platform_operators(), "Platform operators", || {
        let mut seeded: Vec<Member> = operators
            .iter()
            .filter_map(|login| match Principal::user(login) {
                Ok(Principal::User(login)) => Some(Member {
                    member: MemberRef::User(login),
                    role: TeamRole::Member,
                }),
                _ => {
                    tracing::warn!(
                        login,
                        "CONTROLLER_OPERATORS entry is not a valid login; skipped"
                    );
                    None
                }
            })
            .collect();
        for group in operator_groups {
            let rule = if group.starts_with('/') {
                MembershipRule::parse(&format!("group-prefix:{group}"))
            } else {
                MembershipRule::parse(&format!("group-suffix:{group}"))
            };
            match rule {
                Ok(rule) => seeded.push(Member {
                    member: MemberRef::Rule(rule),
                    role: TeamRole::Member,
                }),
                Err(e) => {
                    tracing::warn!(group, error = %e, "CONTROLLER_OPERATOR_GROUPS entry skipped")
                }
            }
        }
        seeded
    })
    .await
}

async fn seed_team(
    pool: &PgPool,
    slug: &TeamSlug,
    display_name: &str,
    members: impl FnOnce() -> Vec<Member>,
) -> Result<SeedOutcome> {
    let now = crate::clock::now_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("opening the {slug} seed"))?;
    if store::get_team(&mut *tx, slug).await?.is_none() {
        store::insert_team(&mut tx, slug, display_name, None, &now)
            .await
            .with_context(|| format!("creating the {slug} team"))?;
    }
    let existing = store::all_members(&mut *tx).await?;
    let users = store::known_users(&mut *tx).await?;
    if resolve::reachable(&existing, &users, slug) {
        tx.commit()
            .await
            .with_context(|| format!("committing the {slug} seed"))?;
        return Ok(SeedOutcome::Reachable);
    }
    let members = members();
    if members.is_empty() {
        tx.commit()
            .await
            .with_context(|| format!("committing the {slug} seed"))?;
        return Ok(SeedOutcome::Unreachable);
    }
    let prior = store::members_of(&mut *tx, slug).await?;
    let added = store::add_members_if_absent(&mut tx, slug, &members, &now).await?;
    let result = store::members_of(&mut *tx, slug).await?;
    store::audit(
        &mut tx,
        &AuditEvent {
            actor: "configuration".into(),
            subject: None,
            auth_path: "startup".into(),
            action: "team:manage-members".into(),
            resource_type: "team".into(),
            resource_id: slug.to_string(),
            allowed: true,
            rule: "startup-seed".into(),
            prior: Some(members_json(&prior)),
            result: Some(members_json(&result)),
        },
        &now,
    )
    .await?;
    tx.commit()
        .await
        .with_context(|| format!("committing the {slug} seed"))?;
    Ok(SeedOutcome::Seeded { added })
}

/// Which policy set the process runs after startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyOutcome {
    /// The stored active set loaded.
    Stored { digest: String },
    /// No set was active; the shipped default was stored and activated.
    DefaultStored { digest: String },
    /// The active set was an earlier shipped default nobody had activated by hand; the shipped
    /// default replaced it.
    DefaultUpgraded { from: String, digest: String },
    /// The stored active set no longer loads; the default runs unstored until an administrator
    /// activates a set that does.
    Fallback { digest: String, error: String },
}

/// Store and activate the shipped default in place of `prior`, with the audit row.
async fn activate_default(
    conn: &mut sqlx::PgConnection,
    prior: Option<&str>,
    now: &str,
) -> Result<String> {
    let digest = policy::digest(policy::DEFAULT_POLICY);
    store::insert_policy_set(
        conn,
        &digest,
        policy::DEFAULT_POLICY,
        SCHEMA_VERSION,
        None,
        now,
    )
    .await?;
    store::activate_policy_set(conn, &digest, None, now).await?;
    store::audit(
        conn,
        &AuditEvent {
            actor: "configuration".into(),
            subject: None,
            auth_path: "startup".into(),
            action: "policy_set:activate".into(),
            resource_type: "policy_set".into(),
            resource_id: digest.clone(),
            allowed: true,
            rule: match prior {
                Some(_) => "startup-upgrade".into(),
                None => "startup-seed".into(),
            },
            prior: prior.map(|p| serde_json::json!({"digest": p})),
            result: Some(serde_json::json!({"digest": digest})),
        },
        now,
    )
    .await?;
    Ok(digest)
}

/// Load the active policy set from the store into `active`. The shipped default is stored and
/// activated when nothing is active yet, and replaces an earlier shipped default that startup
/// seeded and no administrator has activated a set over.
pub async fn load_active_policy(pool: &PgPool, active: &ActivePolicy) -> Result<PolicyOutcome> {
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("opening the policy load")?;
    let shipped = policy::digest(policy::DEFAULT_POLICY);
    let outcome = match store::active_policy_set(&mut *tx).await? {
        Some(row)
            if row.created_by.is_none() && row.activated_by.is_none() && row.digest != shipped =>
        {
            let digest = activate_default(&mut tx, Some(&row.digest), &now).await?;
            active.swap(Engine::load(policy::DEFAULT_POLICY)?);
            PolicyOutcome::DefaultUpgraded {
                from: row.digest,
                digest,
            }
        }
        Some(row) => match Engine::load(&row.text) {
            Ok(engine) => {
                let digest = engine.digest().to_string();
                active.swap(engine);
                PolicyOutcome::Stored { digest }
            }
            Err(e) => PolicyOutcome::Fallback {
                digest: row.digest,
                error: e.to_string(),
            },
        },
        None => PolicyOutcome::DefaultStored {
            digest: activate_default(&mut tx, None, &now).await?,
        },
    };
    tx.commit().await.context("committing the policy load")?;
    Ok(outcome)
}

/// Move every `group:<path>` owner to a team whose sole member is that group at `owner`
/// (RFC-0003 C-COMPATIBILITY). The slug is the path's last segment lowercased with every character
/// outside `[a-z0-9-]` replaced by `-`, suffixed `-2`, `-3`, ... while taken by a team that is not
/// already this group's. Idempotent: a second pass finds no group owners. Returns the number of
/// rows re-owned.
pub async fn migrate_group_owners(pool: &PgPool) -> Result<u64> {
    const OWNER_COLUMNS: [(&str, &str, &str, &str); 8] = [
        ("secrets", "owner", "id", "secret"),
        ("model_providers", "secret_owner", "id", "model_provider"),
        (
            "playbook_standing_launches",
            "owner_principal",
            "id",
            "standing_launch",
        ),
        ("playbooks", "owner", "id", "playbook"),
        ("playbook_drafts", "owner", "id", "playbook_draft"),
        ("pack_imports", "owner", "id", "pack_import"),
        ("model_providers", "owner", "id", "model_provider"),
        ("policy_sets", "owner", "digest", "policy_set"),
    ];
    let now = crate::clock::now_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .context("opening the group owner migration")?;
    let mut groups: Vec<String> = Vec::new();
    for (table, column, _, _) in OWNER_COLUMNS {
        let found: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT DISTINCT {column} FROM {table} WHERE {column} LIKE 'group:%'"
        ))
        .fetch_all(&mut *tx)
        .await
        .with_context(|| format!("listing group owners in {table}"))?;
        groups.extend(found);
    }
    groups.sort();
    groups.dedup();
    let mut moved = 0;
    for raw in groups {
        let Ok(group @ Principal::Group(_)) = Principal::parse(&raw) else {
            tracing::warn!(owner = raw, "group owner does not parse; left as is");
            continue;
        };
        let slug = team_for_group(&mut tx, &group, &now).await?;
        let team = Principal::Team(slug.clone());
        for (table, column, id_column, resource_type) in OWNER_COLUMNS {
            let ids: Vec<String> = sqlx::query_scalar(&format!(
                "UPDATE {table} SET {column} = $2 WHERE {column} = $1 RETURNING {id_column}"
            ))
            .bind(&raw)
            .bind(team.to_string())
            .fetch_all(&mut *tx)
            .await
            .with_context(|| format!("re-owning {table} rows from {raw}"))?;
            for id in ids {
                moved += 1;
                store::audit(
                    &mut tx,
                    &AuditEvent {
                        actor: "configuration".into(),
                        subject: None,
                        auth_path: "startup".into(),
                        action: format!("{resource_type}:transfer"),
                        resource_type: resource_type.into(),
                        resource_id: id,
                        allowed: true,
                        rule: "group-owner-migration".into(),
                        prior: Some(serde_json::json!({"owner": raw})),
                        result: Some(serde_json::json!({"owner": team.to_string()})),
                    },
                    &now,
                )
                .await?;
            }
        }
    }
    tx.commit()
        .await
        .context("committing the group owner migration")?;
    Ok(moved)
}

/// The team that stands for `group`: an existing team whose sole member is the group at `owner`,
/// else a new one at the first free derived slug.
async fn team_for_group(
    conn: &mut sqlx::PgConnection,
    group: &Principal,
    now: &str,
) -> Result<TeamSlug> {
    let Principal::Group(path) = group else {
        anyhow::bail!("{group} is not a group");
    };
    let segment = path.rsplit('/').next().unwrap_or(path);
    let base: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = match TeamSlug::parse(&base) {
        Ok(slug) => slug.to_string(),
        Err(_) => format!("group-{base}"),
    };
    let sole_member = Member {
        member: MemberRef::Group(path.clone()),
        role: TeamRole::Owner,
    };
    let members = store::all_members(&mut *conn).await?;
    for n in 1u32.. {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let slug = TeamSlug::parse(&candidate)
            .with_context(|| format!("deriving a team slug from {group}"))?;
        match store::get_team(&mut *conn, &slug).await? {
            None => {
                store::insert_team(conn, &slug, segment, None, now)
                    .await
                    .with_context(|| format!("creating team {slug} for {group}"))?;
                let rows = store::replace_members(conn, &slug, &[sole_member], None, now).await?;
                store::audit(
                    conn,
                    &AuditEvent {
                        actor: "configuration".into(),
                        subject: None,
                        auth_path: "startup".into(),
                        action: "team:create".into(),
                        resource_type: "team".into(),
                        resource_id: slug.to_string(),
                        allowed: true,
                        rule: "group-owner-migration".into(),
                        prior: None,
                        result: Some(serde_json::json!({
                            "display_name": segment,
                            "members": members_json(&rows),
                            "group": group.to_string(),
                        })),
                    },
                    now,
                )
                .await?;
                return Ok(slug);
            }
            Some(_) => {
                let mine: Vec<&store::MemberRow> =
                    members.iter().filter(|m| m.team == slug).collect();
                let is_this_group = mine.len() == 1
                    && mine[0].member == sole_member.member
                    && mine[0].role == TeamRole::Owner;
                if is_this_group {
                    return Ok(slug);
                }
            }
        }
    }
    anyhow::bail!("no free team slug derived from {group}")
}

pub(crate) fn members_json(rows: &[store::MemberRow]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|r| {
                serde_json::json!({
                    "kind": r.member.kind(),
                    "member": r.member.stored(),
                    "role": r.role,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::model::{PLATFORM_ADMINISTRATORS, PLATFORM_OPERATORS};
    use sqlx::PgPool;

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_fresh_install_seeds_owners_from_configuration_once(pool: PgPool) {
        let admins = vec!["Will".to_string(), "not a login".to_string()];
        assert_eq!(
            seed_platform_administrators(&pool, &admins)
                .await
                .expect("seed"),
            SeedOutcome::Seeded { added: 1 }
        );
        let slug = TeamSlug::parse(PLATFORM_ADMINISTRATORS).expect("slug");
        let rows = store::members_of(&pool, &slug).await.expect("members");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].member, MemberRef::User("will".into()));
        assert_eq!(rows[0].role, TeamRole::Owner);
        let trail = store::audit_for(&pool, "team", PLATFORM_ADMINISTRATORS, 10)
            .await
            .expect("audit");
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].actor, "configuration");

        // A later boot with a changed list leaves a reachable team alone.
        let admins = vec!["someone-else".to_string()];
        assert_eq!(
            seed_platform_administrators(&pool, &admins)
                .await
                .expect("seed"),
            SeedOutcome::Reachable
        );
        assert_eq!(
            store::members_of(&pool, &slug)
                .await
                .expect("members")
                .len(),
            1
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_empty_list_leaves_the_team_unreachable(pool: PgPool) {
        assert_eq!(
            seed_platform_administrators(&pool, &[])
                .await
                .expect("seed"),
            SeedOutcome::Unreachable
        );
        let slug = TeamSlug::parse(PLATFORM_ADMINISTRATORS).expect("slug");
        assert!(store::get_team(&pool, &slug).await.expect("team").is_some());
        assert!(
            store::members_of(&pool, &slug)
                .await
                .expect("members")
                .is_empty()
        );
        // Once configuration names someone, the next boot fills it.
        assert_eq!(
            seed_platform_administrators(&pool, &["alice".into()])
                .await
                .expect("seed"),
            SeedOutcome::Seeded { added: 1 }
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn operators_seed_as_members_and_groups_as_suffix_rules(pool: PgPool) {
        let outcome = seed_platform_operators(
            &pool,
            &["Bob".to_string()],
            &["team-x".to_string(), "/groups/llm-d".to_string()],
        )
        .await
        .expect("seed");
        assert_eq!(outcome, SeedOutcome::Seeded { added: 3 });
        let slug = TeamSlug::parse(PLATFORM_OPERATORS).expect("slug");
        let rows = store::members_of(&pool, &slug).await.expect("members");
        let spelled: Vec<String> = rows.iter().map(|r| r.member.stored()).collect();
        assert_eq!(
            spelled,
            vec!["group-prefix:/groups/llm-d", "group-suffix:team-x", "bob"]
        );
        assert!(rows.iter().all(|r| r.role == TeamRole::Member));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_default_policy_is_stored_and_activated_once(pool: PgPool) {
        let active = ActivePolicy::default_set().expect("default");
        let digest = policy::digest(policy::DEFAULT_POLICY);
        assert_eq!(
            load_active_policy(&pool, &active).await.expect("load"),
            PolicyOutcome::DefaultStored {
                digest: digest.clone()
            }
        );
        let row = store::active_policy_set(&pool)
            .await
            .expect("active")
            .expect("row");
        assert_eq!(row.digest, digest);
        assert_eq!(row.activated_by, None);
        assert_eq!(
            load_active_policy(&pool, &active).await.expect("load"),
            PolicyOutcome::Stored { digest }
        );

        let mut conn = pool.acquire().await.expect("conn");
        let now = crate::clock::now_rfc3339();
        store::insert_policy_set(
            &mut conn,
            "broken",
            "not cedar",
            SCHEMA_VERSION,
            Some("user:root"),
            &now,
        )
        .await
        .expect("insert");
        store::activate_policy_set(&mut conn, "broken", Some("user:root"), &now)
            .await
            .expect("activate");
        let outcome = load_active_policy(&pool, &active).await.expect("load");
        assert!(
            matches!(outcome, PolicyOutcome::Fallback { ref digest, .. } if digest == "broken"),
            "{outcome:?}"
        );
        assert_eq!(
            active.current().digest(),
            policy::digest(policy::DEFAULT_POLICY)
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_earlier_seeded_default_is_upgraded_and_a_chosen_set_is_left_alone(pool: PgPool) {
        let active = ActivePolicy::default_set().expect("default");
        let shipped = policy::digest(policy::DEFAULT_POLICY);
        let earlier = "@id(\"platform-admin-all\")\npermit(principal, action, resource)\nwhen { principal.platform_admin };";
        let earlier_digest = policy::digest(earlier);
        let now = crate::clock::now_rfc3339();
        {
            let mut conn = pool.acquire().await.expect("conn");
            store::insert_policy_set(
                &mut conn,
                &earlier_digest,
                earlier,
                SCHEMA_VERSION,
                None,
                &now,
            )
            .await
            .expect("insert");
            store::activate_policy_set(&mut conn, &earlier_digest, None, &now)
                .await
                .expect("activate");
        }
        assert_eq!(
            load_active_policy(&pool, &active).await.expect("load"),
            PolicyOutcome::DefaultUpgraded {
                from: earlier_digest.clone(),
                digest: shipped.clone(),
            }
        );
        assert_eq!(active.current().digest(), shipped);
        let row = store::active_policy_set(&pool)
            .await
            .expect("active")
            .expect("row");
        assert_eq!(row.digest, shipped);
        let trail = store::audit_for(&pool, "policy_set", &shipped, 10)
            .await
            .expect("trail");
        assert_eq!(trail.len(), 1, "{trail:?}");
        assert_eq!(trail[0].rule, "startup-upgrade");
        assert_eq!(
            trail[0].prior,
            Some(serde_json::json!({"digest": earlier_digest}))
        );
        assert_eq!(
            load_active_policy(&pool, &active).await.expect("load"),
            PolicyOutcome::Stored {
                digest: shipped.clone()
            },
            "the upgrade runs once"
        );

        let mut conn = pool.acquire().await.expect("conn");
        store::activate_policy_set(&mut conn, &earlier_digest, Some("user:root"), &now)
            .await
            .expect("activate");
        assert_eq!(
            load_active_policy(&pool, &active).await.expect("load"),
            PolicyOutcome::Stored {
                digest: earlier_digest.clone()
            },
            "a set an administrator activated is theirs to keep"
        );
        assert_eq!(active.current().digest(), earlier_digest);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn group_owners_move_to_a_team_per_group_and_the_pass_is_idempotent(pool: PgPool) {
        let now = crate::clock::now_rfc3339();
        for (id, owner) in [
            ("s1", "group:/groups/llm-d"),
            ("s2", "group:/groups/llm-d"),
            ("s3", "group:/corp/LLM-D"),
            ("s4", "user:alice"),
        ] {
            sqlx::query(
                "INSERT INTO secrets (id, name, owner, kind, visibility, consumer, mode, \
                                      vault_path, created_at, updated_at) \
                 VALUES ($1, $1, $2, 'opaque', 'broker_only', 'run', 'managed', 'p', $3, $3)",
            )
            .bind(id)
            .bind(owner)
            .bind(&now)
            .execute(&pool)
            .await
            .expect("secret");
        }
        sqlx::query(
            "INSERT INTO model_providers (id, display_name, kind, default_model, secret_name, \
                                          secret_owner, created_by, created_at, updated_at) \
             VALUES ('p1', 'P', 'anthropic', 'm', 's1', 'group:/groups/llm-d', 'alice', $1, $1)",
        )
        .bind(&now)
        .execute(&pool)
        .await
        .expect("provider");
        // A team already holding the derived slug that is not this group's.
        let mut conn = pool.acquire().await.expect("conn");
        let taken = TeamSlug::parse("llm-d").expect("slug");
        store::insert_team(&mut conn, &taken, "Taken", None, &now)
            .await
            .expect("team");
        store::replace_members(
            &mut conn,
            &taken,
            &[Member {
                member: MemberRef::User("bob".into()),
                role: TeamRole::Owner,
            }],
            None,
            &now,
        )
        .await
        .expect("members");

        assert_eq!(migrate_group_owners(&pool).await.expect("migrate"), 4);
        let owners: Vec<(String, String)> =
            sqlx::query_as("SELECT id, owner FROM secrets ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("owners");
        assert_eq!(
            owners,
            vec![
                ("s1".into(), "team:llm-d-3".into()),
                ("s2".into(), "team:llm-d-3".into()),
                ("s3".into(), "team:llm-d-2".into()),
                ("s4".into(), "user:alice".into()),
            ]
        );
        let provider_owner: String =
            sqlx::query_scalar("SELECT secret_owner FROM model_providers WHERE id = 'p1'")
                .fetch_one(&pool)
                .await
                .expect("provider");
        assert_eq!(provider_owner, "team:llm-d-3");
        let members = store::members_of(&pool, &TeamSlug::parse("llm-d-3").expect("slug"))
            .await
            .expect("members");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].member, MemberRef::Group("/groups/llm-d".into()));
        assert_eq!(members[0].role, TeamRole::Owner);
        let trail = store::audit_for(&pool, "secret", "s1", 10)
            .await
            .expect("trail");
        assert_eq!(trail[0].rule, "group-owner-migration");
        assert_eq!(trail[0].action, "secret:transfer");

        assert_eq!(migrate_group_owners(&pool).await.expect("again"), 0);
        assert_eq!(store::list_teams(&pool).await.expect("teams").len(), 3);
    }
}

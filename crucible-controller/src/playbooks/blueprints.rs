//! Blueprints (RFC-0006): registered playbooks a user starts a draft of their own from. The
//! owning team's secrets carry into the draft for the team's members; every other declared secret
//! becomes a placeholder the user fills with a bind of their own.

use crate::authz::model::Principal;
use crate::playbooks::registry::PlaybookRow;
use crate::secrets::manifest::DeclaredSecret;
use crate::secrets::store::{BindingRow, SecretRow};
use crate::secrets::{ProjectionKind, SecretName};
use crate::wire_enum::wire_enum;
use anyhow::{Context, Result};
use sqlx::{PgConnection, PgPool, Row};

/// Why a declared secret did not carry, in the order the reasons are tested.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum PlaceholderReason {
    /// The blueprint binds nothing under the name.
    Unbound,
    /// The blueprint's binding only serves a reviewed revision.
    Pinned,
    /// The bound secret belongs to a user.
    PersonalSecret,
    /// The bound secret does not belong to the blueprint's owner.
    OtherOwner,
    /// The user is not in the team that owns the secret.
    NotInTeam,
    /// The active policy set does not let the user bind it.
    PolicyDenied,
    /// Its reference or stored value did not verify for this user.
    Unverified,
}

wire_enum!(PlaceholderReason, "placeholder reason", both, {
    PlaceholderReason::Unbound => "unbound",
    PlaceholderReason::Pinned => "pinned",
    PlaceholderReason::PersonalSecret => "personal_secret",
    PlaceholderReason::OtherOwner => "other_owner",
    PlaceholderReason::NotInTeam => "not_in_team",
    PlaceholderReason::PolicyDenied => "policy_denied",
    PlaceholderReason::Unverified => "unverified",
});

/// A declared secret a draft started from a blueprint still has to bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    pub declared_name: SecretName,
    pub projection: Option<(ProjectionKind, String)>,
    pub reason: PlaceholderReason,
}

/// Why `binding` on `blueprint` carries to nobody, or `None` when it carries to the members of the
/// team that owns both.
pub fn barred(
    blueprint: &PlaybookRow,
    binding: &BindingRow,
    secret: &SecretRow,
) -> Option<PlaceholderReason> {
    if binding.pack_rev.is_some() {
        return Some(PlaceholderReason::Pinned);
    }
    if matches!(secret.owner, Principal::User(_)) {
        return Some(PlaceholderReason::PersonalSecret);
    }
    if !matches!(blueprint.owner, Principal::Team(_)) || secret.owner != blueprint.owner {
        return Some(PlaceholderReason::OtherOwner);
    }
    None
}

/// The secrets the blueprint's registered revision declares, under its registered variant.
pub async fn declared(pool: &PgPool, blueprint: &PlaybookRow) -> Result<Vec<DeclaredSecret>> {
    let digest = blueprint.tree_digest.as_ref().map(ToString::to_string);
    let tree = crate::playbooks::registry::pack_at_rev(
        pool,
        &blueprint.id,
        &blueprint.rev,
        digest.as_deref(),
    )
    .await?
    .with_context(|| format!("blueprint {} moved while it was read", blueprint.id))?;
    let files = crate::playbooks::packs::text_files(tree)?;
    let Some(manifest) = files.get(crate::playbooks::packs::MANIFEST) else {
        return Ok(Vec::new());
    };
    let text = crucible::manifest::variant::effective_text(manifest, blueprint.variant.as_ref())
        .with_context(|| format!("reading the manifest of blueprint {}", blueprint.id))?;
    crate::secrets::manifest::parse_declared(&text)
}

/// Every secret name `manifest` declares, in its base or under any variant.
pub fn declared_anywhere(manifest: &str) -> Result<Vec<SecretName>> {
    let variants = crucible::manifest::variant::declared(manifest)?;
    let mut names = std::collections::BTreeSet::new();
    for variant in std::iter::once(None).chain(variants.iter().map(Some)) {
        let text = crucible::manifest::variant::effective_text(manifest, variant)?;
        let declared = crate::secrets::manifest::parse_declared(&text)?;
        names.extend(declared.into_iter().map(|d| d.name));
    }
    Ok(names.into_iter().collect())
}

/// Record `placeholders` on draft `draft`.
pub async fn insert_placeholders(
    conn: &mut PgConnection,
    draft: &str,
    placeholders: &[Placeholder],
) -> Result<()> {
    for p in placeholders {
        let (kind, projection) = p.projection.clone().unzip();
        sqlx::query(
            "INSERT INTO draft_placeholders
                 (draft_id, declared_name, projection_kind, projection, reason)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(draft)
        .bind(p.declared_name.as_str())
        .bind(kind.map(ProjectionKind::as_str))
        .bind(projection)
        .bind(p.reason.as_str())
        .execute(&mut *conn)
        .await
        .context("recording a placeholder")?;
    }
    Ok(())
}

/// The placeholders draft `draft` has yet to fill, by declared name.
pub async fn placeholders(pool: &PgPool, draft: &str) -> Result<Vec<Placeholder>> {
    let rows = sqlx::query(
        "SELECT declared_name, projection_kind, projection, reason FROM draft_placeholders
         WHERE draft_id = $1 ORDER BY declared_name",
    )
    .bind(draft)
    .fetch_all(pool)
    .await
    .context("listing a draft's placeholders")?;
    rows.iter()
        .map(|row| {
            let kind: Option<ProjectionKind> = row.try_get("projection_kind")?;
            let projection: Option<String> = row.try_get("projection")?;
            Ok(Placeholder {
                declared_name: SecretName::parse(&row.try_get::<String, _>("declared_name")?)?,
                projection: kind.zip(projection),
                reason: row.try_get("reason")?,
            })
        })
        .collect()
}

/// Drop draft `draft`'s placeholder for `declared_name`, once something binds it.
pub async fn fill_placeholder(
    conn: &mut PgConnection,
    draft: &str,
    declared_name: &SecretName,
) -> Result<()> {
    sqlx::query("DELETE FROM draft_placeholders WHERE draft_id = $1 AND declared_name = $2")
        .bind(draft)
        .bind(declared_name.as_str())
        .execute(conn)
        .await
        .context("filling a placeholder")?;
    Ok(())
}

/// Drop draft `draft`'s placeholders for names its pack no longer declares.
pub async fn drop_undeclared(
    conn: &mut PgConnection,
    draft: &str,
    declared: &[SecretName],
) -> Result<()> {
    let names: Vec<&str> = declared.iter().map(SecretName::as_str).collect();
    sqlx::query(
        "DELETE FROM draft_placeholders WHERE draft_id = $1 AND NOT declared_name = ANY($2)",
    )
    .bind(draft)
    .bind(&names)
    .execute(conn)
    .await
    .context("dropping undeclared placeholders")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::playbooks::blueprints::*;
    use crate::playbooks::registry::PlaybookSource;
    use crate::secrets::{ConsumerClass, ScopeKind, SecretKind, SecretMode, Visibility};

    fn principal(p: &str) -> Principal {
        Principal::parse(p).expect("principal")
    }

    fn blueprint(owner: &str) -> PlaybookRow {
        PlaybookRow {
            id: "pack".to_string(),
            description: "d".to_string(),
            source: PlaybookSource::Draft {
                draft: "studio".to_string(),
                version: 1,
            },
            rev: "tree1:aa".to_string(),
            tar_digest: String::new(),
            tree_digest: None,
            schema_digest: "sha256:aa".to_string(),
            agent: None,
            core_rev: String::new(),
            exposure_digest: None,
            owner: principal(owner),
            created_by: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
            variant: None,
            blueprint: true,
            public: false,
        }
    }

    fn binding(pack_rev: Option<&str>) -> BindingRow {
        BindingRow {
            id: "b".to_string(),
            secret_id: "s".to_string(),
            scope_kind: ScopeKind::Playbook,
            scope_id: "pack".to_string(),
            projection_kind: ProjectionKind::Env,
            projection: "TOKEN".to_string(),
            declared_name: SecretName::parse("token").expect("name"),
            pack_rev: pack_rev.map(str::to_string),
            pack_digest: None,
            schema_digest: None,
            created_by: None,
            created_at: "now".to_string(),
            origin: None,
            origin_binding: None,
        }
    }

    fn secret(owner: &str) -> SecretRow {
        SecretRow {
            id: "s".to_string(),
            name: SecretName::parse("token").expect("name"),
            owner: principal(owner),
            kind: SecretKind::Opaque,
            visibility: Visibility::BrokerOnly,
            consumer: ConsumerClass::Run,
            mode: SecretMode::Managed,
            vault_path: "x".to_string(),
            current_version: Some(1),
            created_by: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        }
    }

    #[test]
    fn only_an_unpinned_binding_of_the_owning_teams_secret_carries() {
        let team = blueprint("team:mlr");
        let open = binding(None);
        assert_eq!(barred(&team, &open, &secret("team:mlr")), None);
        assert_eq!(
            barred(&team, &binding(Some("tree1:aa")), &secret("team:mlr")),
            Some(PlaceholderReason::Pinned)
        );
        assert_eq!(
            barred(&team, &open, &secret("user:reed")),
            Some(PlaceholderReason::PersonalSecret)
        );
        assert_eq!(
            barred(&team, &open, &secret("team:other")),
            Some(PlaceholderReason::OtherOwner)
        );
        assert_eq!(
            barred(&blueprint("user:reed"), &open, &secret("team:mlr")),
            Some(PlaceholderReason::OtherOwner),
            "a personal blueprint carries nothing"
        );
        assert_eq!(
            barred(&blueprint("user:reed"), &open, &secret("user:reed")),
            Some(PlaceholderReason::PersonalSecret)
        );
    }

    #[test]
    fn a_name_any_variant_declares_counts() {
        let manifest = "[[secret]]\nname = \"token\"\n\n[[variant.offline.secret]]\nname = \"fixture\"\n\n[variant.e2e.workspace]\ndir = \"e2e\"\n";
        let names: Vec<String> = declared_anywhere(manifest)
            .expect("parses")
            .into_iter()
            .map(|n| n.to_string())
            .collect();
        assert_eq!(names, vec!["fixture", "token"]);
        assert!(declared_anywhere("[[secret]]\nname = \"a b\"\n").is_err());
    }

    fn placeholder(name: &str, reason: PlaceholderReason) -> Placeholder {
        Placeholder {
            declared_name: SecretName::parse(name).expect("name"),
            projection: Some((ProjectionKind::Env, name.to_uppercase())),
            reason,
        }
    }

    async fn listed(pool: &PgPool, draft: &str) -> Vec<String> {
        placeholders(pool, draft)
            .await
            .expect("placeholders")
            .into_iter()
            .map(|p| p.declared_name.to_string())
            .collect()
    }

    /// Placeholders belong to the draft: filled by a bind, dropped when undeclared, renamed and
    /// deleted with it.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn placeholders_follow_their_draft(pool: PgPool) {
        let owner = principal("user:kim");
        crate::playbooks::drafts::create(
            &pool,
            "copy",
            "d",
            crate::playbooks::drafts::DraftSeed::Skeleton,
            None,
            &owner,
        )
        .await
        .expect("draft");
        let mut conn = pool.acquire().await.expect("conn");
        let unbound = Placeholder {
            declared_name: SecretName::parse("unbound").expect("name"),
            projection: None,
            reason: PlaceholderReason::Unbound,
        };
        insert_placeholders(
            &mut conn,
            "copy",
            &[
                placeholder("alpha", PlaceholderReason::PersonalSecret),
                placeholder("beta", PlaceholderReason::Pinned),
                placeholder("gamma", PlaceholderReason::NotInTeam),
                unbound.clone(),
            ],
        )
        .await
        .expect("insert");
        let read = placeholders(&pool, "copy").await.expect("read");
        assert_eq!(
            read[0],
            placeholder("alpha", PlaceholderReason::PersonalSecret)
        );
        assert_eq!(read[3], unbound, "no projection reads back as none");

        fill_placeholder(
            &mut conn,
            "copy",
            &SecretName::parse("alpha").expect("name"),
        )
        .await
        .expect("fill");
        drop_undeclared(
            &mut conn,
            "copy",
            &[
                SecretName::parse("beta").expect("name"),
                SecretName::parse("gamma").expect("name"),
            ],
        )
        .await
        .expect("drop");
        assert_eq!(listed(&pool, "copy").await, vec!["beta", "gamma"]);
        drop(conn);

        crate::playbooks::drafts::rename(&pool, "copy", "renamed", None, None)
            .await
            .expect("rename");
        assert!(listed(&pool, "copy").await.is_empty());
        assert_eq!(listed(&pool, "renamed").await, vec!["beta", "gamma"]);
        crate::playbooks::drafts::delete(&pool, "renamed", None, None)
            .await
            .expect("delete")
            .expect("deleted");
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM draft_placeholders")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(left, 0);
    }
}

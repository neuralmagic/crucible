//! Who a person is on each cluster the deploy profile knows. People's access tokens carry the
//! answer as `kube_user`, and the Settings page shows it.
//!
//! On the hub an administrator's override wins. Otherwise a `{ claim, prefix }` cluster takes the
//! claim the person's SSO login recorded, and a `link` cluster takes only what the person proved
//! to that cluster.
//!
//! Issuance and grant records name who acts as an [`authz::model::Principal`]: [`for_principal`]
//! resolves a person through their sign-in and a run through the person it was dispatched for, and
//! refuses a group or a team, which have no username of their own yet. A team's own identity per
//! cluster would be one more arm there.
//!
//! [`authz::model::Principal`]: crate::authz::model::Principal

use crate::authz::model::Principal;
use crate::identity::kube_identity::KubeUser;
use crate::identity::kube_links::{self, LinkSource};
use crate::runs::clusters::HUB_CLUSTER;
use anyhow::Context;
use forge::fleet::{ClaimIdentity, ClusterIdentity, SsoClaim};
use sqlx::PgPool;

/// The signed-in person, as their login recorded them.
#[derive(Debug, Clone, Copy)]
pub struct Person<'a> {
    pub sub: &'a str,
    pub login: &'a str,
    pub email: Option<&'a str>,
}

/// Where a resolved username came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Rule,
    Link(LinkSource),
    Override,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Known {
        user: KubeUser,
        source: Source,
        linked_at: Option<String>,
    },
    /// A `link` cluster the person has not linked.
    Unlinked,
    /// The rule names a claim this login did not carry, or one that is no username.
    Unavailable(String),
}

/// The username a `{ claim, prefix }` rule gives `person` on `cluster`.
pub fn from_claim(rule: &ClaimIdentity, cluster: &str, person: Person<'_>) -> Resolution {
    let (name, value) = match rule.claim {
        SsoClaim::Login => ("login", Some(person.login)),
        SsoClaim::Email => ("email", person.email),
    };
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return Resolution::Unavailable(format!(
            "cluster {cluster} takes usernames from the {name} claim, which your login did not carry"
        ));
    };
    match KubeUser::parse(&format!("{}{value}", rule.prefix)) {
        Ok(user) => Resolution::Known {
            user,
            source: Source::Rule,
            linked_at: None,
        },
        Err(e) => Resolution::Unavailable(e.to_string()),
    }
}

/// `person`'s username on `cluster` under `rule`.
pub async fn resolve(
    pool: &PgPool,
    cluster: &str,
    rule: &ClusterIdentity,
    person: Person<'_>,
) -> anyhow::Result<Resolution> {
    if cluster == HUB_CLUSTER
        && let Some(user) =
            crate::identity::kube_identity::override_for_sub(pool, person.sub).await?
    {
        return Ok(Resolution::Known {
            user,
            source: Source::Override,
            linked_at: None,
        });
    }
    let ClusterIdentity::Claim(rule) = rule else {
        let link = kube_links::list(pool, person.sub)
            .await?
            .into_iter()
            .find(|l| l.cluster == cluster);
        return Ok(match link {
            Some(link) => Resolution::Known {
                user: link.kube_user,
                source: Source::Link(link.source),
                linked_at: Some(link.linked_at),
            },
            None => Resolution::Unlinked,
        });
    };
    Ok(from_claim(rule, cluster, person))
}

/// Why a principal has no username on a cluster to resolve.
#[derive(Debug, thiserror::Error)]
pub enum PrincipalError {
    #[error(
        "{principal} has no username of its own on a cluster; only a person, or a run through the \
         person it was dispatched for, has one"
    )]
    NoUsername { principal: String },
    #[error("run {run_id} was dispatched for {dispatched_by:?}, not a person")]
    Dispatcher {
        run_id: String,
        dispatched_by: Option<String>,
    },
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

/// The username `principal` acts as on `cluster`, under the deploy profile's rule for it: a
/// person's own, a run's dispatcher's. A login with no sign-in recorded has none.
pub async fn for_principal(
    pool: &PgPool,
    profile: &crucible::deploy::DeployProfile,
    cluster: &str,
    principal: &Principal,
) -> Result<Resolution, PrincipalError> {
    match principal {
        Principal::User(login) => Ok(for_login(pool, profile, cluster, login).await?),
        Principal::Run(run_id) => {
            let dispatched_by: Option<String> =
                sqlx::query_scalar("SELECT dispatched_by FROM runs WHERE run_id = $1")
                    .bind(run_id)
                    .fetch_optional(pool)
                    .await
                    .context("reading who the run was dispatched for")?
                    .flatten();
            let dispatcher = dispatched_by.as_deref().map(Principal::parse);
            match dispatcher {
                Some(Ok(Principal::User(login))) => {
                    Ok(for_login(pool, profile, cluster, &login).await?)
                }
                _ => Err(PrincipalError::Dispatcher {
                    run_id: run_id.clone(),
                    dispatched_by,
                }),
            }
        }
        Principal::Group(_) | Principal::Team(_) => Err(PrincipalError::NoUsername {
            principal: principal.to_string(),
        }),
    }
}

async fn for_login(
    pool: &PgPool,
    profile: &crucible::deploy::DeployProfile,
    cluster: &str,
    login: &str,
) -> anyhow::Result<Resolution> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT sub, email FROM users WHERE login = $1")
            .bind(login.trim().to_lowercase())
            .fetch_optional(pool)
            .await
            .context("reading the launcher's sign-in")?;
    let Some((sub, email)) = row else {
        return Ok(Resolution::Unavailable(format!(
            "{login} has never signed in, so no username on {cluster} is known"
        )));
    };
    let rule = profile
        .cluster_identities()
        .into_iter()
        .find(|(name, _)| *name == cluster)
        .map(|(_, rule)| rule.clone())
        .unwrap_or_default();
    let person = Person {
        sub: &sub,
        login,
        email: email.as_deref(),
    };
    resolve(pool, cluster, &rule, person).await
}

#[cfg(test)]
mod tests {
    use crate::identity::cluster_identity::*;
    use crate::identity::kube_links::testing::Clusters;

    const ALICE: Person<'static> = Person {
        sub: "sub-alice",
        login: "alice",
        email: Some("alice@example.com"),
    };

    fn claim(claim: SsoClaim, prefix: &str) -> ClusterIdentity {
        ClusterIdentity::Claim(ClaimIdentity {
            claim,
            prefix: prefix.to_string(),
        })
    }

    fn known(user: &str, source: Source) -> Resolution {
        Resolution::Known {
            user: KubeUser::parse(user).unwrap(),
            source,
            linked_at: None,
        }
    }

    async fn signed_in(pool: &PgPool) {
        crate::identity::oidc::users::record_login(
            pool,
            ALICE.sub,
            ALICE.login,
            ALICE.email,
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_claim_rule_reads_the_login_or_the_email_behind_its_prefix(pool: PgPool) {
        let email = claim(SsoClaim::Email, "");
        assert_eq!(
            resolve(&pool, "k8s", &email, ALICE).await.unwrap(),
            known("alice@example.com", Source::Rule)
        );
        let login = claim(SsoClaim::Login, "sso:");
        assert_eq!(
            resolve(&pool, "k8s", &login, ALICE).await.unwrap(),
            known("sso:alice", Source::Rule)
        );
    }

    #[test]
    fn a_claim_the_login_did_not_carry_or_one_that_is_no_username_resolves_to_nothing() {
        let no_email = Person {
            email: None,
            ..ALICE
        };
        let rule = ClaimIdentity {
            claim: SsoClaim::Email,
            prefix: String::new(),
        };
        let Resolution::Unavailable(why) = from_claim(&rule, "k8s", no_email) else {
            panic!("an absent email resolves to nothing");
        };
        assert!(why.contains("k8s") && why.contains("email"), "{why}");
        let system = ClaimIdentity {
            claim: SsoClaim::Login,
            prefix: "system:".into(),
        };
        assert!(matches!(
            from_claim(&system, "east", ALICE),
            Resolution::Unavailable(_)
        ));
        let spaced = ClaimIdentity {
            claim: SsoClaim::Login,
            prefix: "a b:".into(),
        };
        assert!(matches!(
            from_claim(&spaced, "east", ALICE),
            Resolution::Unavailable(_)
        ));
        let empty = Person {
            email: Some(""),
            ..ALICE
        };
        assert!(matches!(
            from_claim(&rule, "k8s", empty),
            Resolution::Unavailable(_)
        ));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_link_cluster_is_unlinked_until_the_cluster_names_the_person(pool: PgPool) {
        assert_eq!(
            resolve(&pool, "ocp", &ClusterIdentity::Link, ALICE)
                .await
                .unwrap(),
            Resolution::Unlinked
        );
        let clusters = Clusters::with(&[("ocp", "t1", "casey")]);
        kube_links::link(
            &pool,
            &clusters,
            ALICE.sub,
            "ocp",
            "t1",
            LinkSource::OpenshiftOauth,
        )
        .await
        .unwrap();
        let Resolution::Known {
            user,
            source,
            linked_at,
        } = resolve(&pool, "ocp", &ClusterIdentity::Link, ALICE)
            .await
            .unwrap()
        else {
            panic!("linked");
        };
        assert_eq!(user.as_str(), "casey");
        assert_eq!(source, Source::Link(LinkSource::OpenshiftOauth));
        assert!(linked_at.is_some());
    }

    async fn run_dispatched_for(pool: &PgPool, run_id: &str, dispatched_by: &str) {
        crate::issues::store::upsert_issue(
            pool,
            &crate::issues::model::NewIssue {
                key: "owner/repo#1".into(),
                repo: "owner/repo".into(),
                priority: 5,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await
        .unwrap();
        crate::runs::store::insert_run(
            pool,
            &crate::NewRun {
                run_id: run_id.into(),
                scope: None,
                issue: Some("owner/repo#1".into()),
                identity_digest: None,
                status: "running".into(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE runs SET dispatched_by = $2 WHERE run_id = $1")
            .bind(run_id)
            .bind(dispatched_by)
            .execute(pool)
            .await
            .unwrap();
    }

    /// A person resolves through their sign-in and a run through the person it was dispatched
    /// for; a group, a team, or a run dispatched for one has no username to resolve.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_principal_resolves_as_a_person_or_through_its_runs_dispatcher(pool: PgPool) {
        signed_in(&pool).await;
        let profile: crucible::deploy::DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "loops"
            rig_namespace = "rig"
            service_account = "crucible-spoke"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "pull"
            [clusters.k8s]
            kubeconfig_secret = "w"
            identity = { claim = "email", prefix = "oidc:" }
            "#,
        )
        .unwrap();
        let resolve = |p: Principal| {
            let (pool, profile) = (pool.clone(), &profile);
            async move { for_principal(&pool, profile, "k8s", &p).await }
        };
        let alice = known("oidc:alice@example.com", Source::Rule);
        assert_eq!(
            resolve(Principal::user("alice").unwrap()).await.unwrap(),
            alice
        );
        run_dispatched_for(&pool, "run-1", "user:alice").await;
        assert_eq!(
            resolve(Principal::Run("run-1".into())).await.unwrap(),
            alice
        );
        run_dispatched_for(&pool, "run-2", "team:platform-administrators").await;
        assert!(matches!(
            resolve(Principal::Run("run-2".into())).await,
            Err(PrincipalError::Dispatcher { .. })
        ));
        assert!(matches!(
            resolve(Principal::Run("no-such-run".into())).await,
            Err(PrincipalError::Dispatcher {
                dispatched_by: None,
                ..
            })
        ));
        for no_username in [
            Principal::platform(),
            Principal::parse("group:/groups/ml").unwrap(),
        ] {
            assert!(matches!(
                resolve(no_username).await,
                Err(PrincipalError::NoUsername { .. })
            ));
        }
        assert!(matches!(
            resolve(Principal::user("bob").unwrap()).await.unwrap(),
            Resolution::Unavailable(_)
        ));
    }

    /// The override is the hub's alone: it beats the hub's rule and link, and no spoke sees it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_administrators_override_wins_on_the_hub_and_only_there(pool: PgPool) {
        signed_in(&pool).await;
        crate::identity::kube_identity::set_override(
            &pool,
            "alice",
            Some(&KubeUser::parse("alice-admin").unwrap()),
        )
        .await
        .unwrap();
        for rule in [ClusterIdentity::Link, claim(SsoClaim::Login, "")] {
            assert_eq!(
                resolve(&pool, HUB_CLUSTER, &rule, ALICE).await.unwrap(),
                known("alice-admin", Source::Override)
            );
        }
        assert_eq!(
            resolve(&pool, "ocp", &ClusterIdentity::Link, ALICE)
                .await
                .unwrap(),
            Resolution::Unlinked
        );
    }
}

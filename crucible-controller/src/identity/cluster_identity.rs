//! Who a person is on each cluster the deploy profile knows. People's access tokens carry the
//! answer as `kube_user`, and the Settings page shows it.
//!
//! On the hub an administrator's override wins. Otherwise a `{ claim, prefix }` cluster takes the
//! claim the person's SSO login recorded, and a `link` cluster takes only what the person proved
//! to that cluster.

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
    let hub = if cluster == HUB_CLUSTER {
        crate::identity::kube_identity::get_for_sub(pool, person.sub).await?
    } else {
        None
    };
    if let Some(raw) = hub.as_ref().and_then(|h| h.r#override.as_deref()) {
        return Ok(Resolution::Known {
            user: KubeUser::parse(raw).context("the stored hub username override")?,
            source: Source::Override,
            linked_at: None,
        });
    }
    let ClusterIdentity::Claim(rule) = rule else {
        let link = kube_links::list(pool, person.sub)
            .await?
            .into_iter()
            .find(|l| l.cluster == cluster);
        if let Some(link) = link {
            return Ok(Resolution::Known {
                user: link.kube_user,
                source: Source::Link(link.source),
                linked_at: Some(link.linked_at),
            });
        }
        return match hub.and_then(|h| h.learned) {
            Some(raw) => Ok(Resolution::Known {
                user: KubeUser::parse(&raw).context("the stored hub username")?,
                source: Source::Link(LinkSource::TokenReview),
                linked_at: None,
            }),
            None => Ok(Resolution::Unlinked),
        };
    };
    Ok(from_claim(rule, cluster, person))
}

/// The deploy profile with its fleet file folded in, `None` without one.
pub async fn load_profile(
    path: Option<&std::path::Path>,
) -> anyhow::Result<Option<crucible::deploy::DeployProfile>> {
    let Some(path) = path.map(std::path::Path::to_path_buf) else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || {
        crucible::deploy::DeployProfile::load_with_fleet(&path, None).map(Some)
    })
    .await
    .context("joining the deploy profile read")?
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
            resolve(&pool, "waldorf", &email, ALICE).await.unwrap(),
            known("alice@example.com", Source::Rule)
        );
        let login = claim(SsoClaim::Login, "sso:");
        assert_eq!(
            resolve(&pool, "waldorf", &login, ALICE).await.unwrap(),
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
        let Resolution::Unavailable(why) = from_claim(&rule, "waldorf", no_email) else {
            panic!("an absent email resolves to nothing");
        };
        assert!(why.contains("waldorf") && why.contains("email"), "{why}");
        let system = ClaimIdentity {
            claim: SsoClaim::Login,
            prefix: "system:".into(),
        };
        assert!(matches!(
            from_claim(&system, "mpp", ALICE),
            Resolution::Unavailable(_)
        ));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_link_cluster_is_unlinked_until_the_cluster_names_the_person(pool: PgPool) {
        assert_eq!(
            resolve(&pool, "pirate", &ClusterIdentity::Link, ALICE)
                .await
                .unwrap(),
            Resolution::Unlinked
        );
        let clusters = Clusters::with(&[("pirate", "t1", "wseaton")]);
        kube_links::link(
            &pool,
            &clusters,
            ALICE.sub,
            "pirate",
            "t1",
            LinkSource::OpenshiftOauth,
        )
        .await
        .unwrap();
        let Resolution::Known {
            user,
            source,
            linked_at,
        } = resolve(&pool, "pirate", &ClusterIdentity::Link, ALICE)
            .await
            .unwrap()
        else {
            panic!("linked");
        };
        assert_eq!(user.as_str(), "wseaton");
        assert_eq!(source, Source::Link(LinkSource::OpenshiftOauth));
        assert!(linked_at.is_some());
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
            resolve(&pool, "pirate", &ClusterIdentity::Link, ALICE)
                .await
                .unwrap(),
            Resolution::Unlinked
        );
    }

    /// A hub username learned before per-cluster links existed still counts as the hub link.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_hub_username_learned_from_a_cluster_token_counts_as_linked(pool: PgPool) {
        signed_in(&pool).await;
        crate::identity::kube_identity::learn_for_sub(
            &pool,
            ALICE.sub,
            &KubeUser::parse("alice-k8s").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            resolve(&pool, HUB_CLUSTER, &ClusterIdentity::Link, ALICE)
                .await
                .unwrap(),
            known("alice-k8s", Source::Link(LinkSource::TokenReview))
        );
    }
}

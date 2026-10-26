//! Render the controller's `OAuthClient` for one cluster from the profile's `[cluster_link]` table.
//! The client is confidential, may ask only for `user:info`, redirects only to the controller's
//! callback for that cluster, and its tokens die quickly. Its secret is never rendered: whoever
//! applies the object sets it on the cluster and in the controller's Secret.

use crate::deploy::profile::{ClusterLinkCfg, DeployProfile, KubeName};
use anyhow::{Context, Result};
use crucible_contract::cluster_link::{ExternalUrl, SCOPE};
use forge::fleet::ClusterIdentity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClusterLinkError {
    #[error(
        "rendering an OAuthClient needs a [cluster_link] table in the deploy profile \\
         (openshift)"
    )]
    MissingTable,
    #[error(
        "[cluster_link] sets no external_url, and no agent issuer was given to default it to \
         (--issuer)"
    )]
    NoBrowserUrl,
    #[error("[cluster_link].openshift does not list {cluster:?}; it lists {listed:?}")]
    NotListed {
        cluster: String,
        listed: Vec<String>,
    },
    #[error(
        "[cluster_link].openshift lists {cluster:?}, which is neither `hub` nor a [clusters.<name>] entry"
    )]
    UnknownCluster { cluster: String },
    #[error(
        "[cluster_link].openshift lists {cluster:?}, whose usernames come from the SSO login; \
         only an `identity = \"link\"` cluster links"
    )]
    NotLinkCluster { cluster: String },
    #[error("--cluster-link renders an object for another cluster; apply it there yourself")]
    NoApply,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OAuthClient {
    api_version: &'static str,
    kind: &'static str,
    metadata: ObjectMeta,
    grant_method: &'static str,
    #[serde(rename = "redirectURIs")]
    redirect_uris: Vec<String>,
    scope_restrictions: Vec<ScopeRestriction>,
    access_token_max_age_seconds: u32,
}

#[derive(Serialize)]
struct ScopeRestriction {
    literals: Vec<&'static str>,
}

/// The `OAuthClient` to create on `cluster`, as YAML. `issuer` is the controller's agent issuer,
/// its browser URL when `[cluster_link]` sets no `external_url`.
pub fn render(profile: &DeployProfile, cluster: &str, issuer: Option<&str>) -> Result<String> {
    let cfg = profile
        .cluster_link
        .as_ref()
        .ok_or(ClusterLinkError::MissingTable)?;
    let cluster = listed(profile, cfg, cluster)?;
    let url = cfg
        .browser_url(issuer)
        .ok_or(ClusterLinkError::NoBrowserUrl)?;
    let callback = ExternalUrl::parse(url)
        .and_then(|url| url.callback(cluster.as_str()))
        .context("[cluster_link].external_url")?;
    let client = OAuthClient {
        api_version: "oauth.openshift.io/v1",
        kind: "OAuthClient",
        metadata: ObjectMeta {
            name: Some(cfg.client_id.to_string()),
            labels: Some(BTreeMap::from([(
                crucible_contract::MANAGED_BY_KEY.to_string(),
                crucible_contract::MANAGED_BY_VALUE.to_string(),
            )])),
            ..ObjectMeta::default()
        },
        grant_method: "auto",
        redirect_uris: vec![callback.to_string()],
        scope_restrictions: vec![ScopeRestriction {
            literals: vec![SCOPE],
        }],
        access_token_max_age_seconds: cfg.access_token_max_age_secs,
    };
    serde_norway::to_string(&client).context("serializing the OAuthClient")
}

fn listed<'a>(
    profile: &DeployProfile,
    cfg: &'a ClusterLinkCfg,
    cluster: &str,
) -> Result<&'a KubeName, ClusterLinkError> {
    let name = cfg
        .openshift
        .iter()
        .find(|c| c.as_str() == cluster)
        .ok_or_else(|| ClusterLinkError::NotListed {
            cluster: cluster.to_string(),
            listed: cfg.openshift.iter().map(KubeName::to_string).collect(),
        })?;
    match profile.cluster_identity(name.as_str()) {
        None => Err(ClusterLinkError::UnknownCluster {
            cluster: name.to_string(),
        }),
        Some(ClusterIdentity::Claim(_)) => Err(ClusterLinkError::NotLinkCluster {
            cluster: name.to_string(),
        }),
        Some(ClusterIdentity::Link) => Ok(name),
    }
}

#[cfg(test)]
mod tests {
    use crate::deploy::cluster_link::*;

    const PROFILE: &str = r#"
        [cluster]
        loop_namespace = "crucible-system"
        rig_namespace = "rig"
        service_account = "crucible-spoke"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
        [clusters.ocp]
        kubeconfig_secret = "ocp-kubeconfig"
        [clusters.k8s]
        kubeconfig_secret = "k8s-kubeconfig"
        identity = { claim = "email" }
        [cluster_link]
        external_url = "https://crucible.apps.hub.example.com"
        openshift = ["ocp", "hub", "gone", "k8s"]
    "#;

    fn profile(text: &str) -> DeployProfile {
        toml::from_str(text).unwrap()
    }

    fn rendered(cluster: &str) -> serde_json::Value {
        serde_norway::from_str(&render(&profile(PROFILE), cluster, None).unwrap()).unwrap()
    }

    #[test]
    fn the_oauth_client_redirects_only_to_this_clusters_callback_and_asks_only_user_info() {
        assert_eq!(
            rendered("ocp"),
            serde_json::json!({
                "apiVersion": "oauth.openshift.io/v1",
                "kind": "OAuthClient",
                "metadata": {
                    "name": "crucible-controller",
                    "labels": {"app.kubernetes.io/managed-by": "crucible"}
                },
                "grantMethod": "auto",
                "redirectURIs": [
                    "https://crucible.apps.hub.example.com/oauth/cluster-link/ocp/callback"
                ],
                "scopeRestrictions": [{"literals": ["user:info"]}],
                "accessTokenMaxAgeSeconds": 300
            })
        );
        assert_eq!(
            rendered("hub")["redirectURIs"][0],
            "https://crucible.apps.hub.example.com/oauth/cluster-link/hub/callback"
        );
    }

    #[test]
    fn no_secret_is_ever_rendered() {
        let yaml = render(&profile(PROFILE), "ocp", None).unwrap();
        assert!(!yaml.contains("secret"), "{yaml}");
    }

    #[test]
    fn the_client_name_and_token_lifetime_follow_the_profile() {
        let text = PROFILE.replace(
            "openshift = [",
            "client_id = \"crucible-staging\"\n        access_token_max_age_secs = 120\n        openshift = [",
        );
        let doc: serde_json::Value =
            serde_norway::from_str(&render(&profile(&text), "ocp", None).unwrap()).unwrap();
        assert_eq!(doc["metadata"]["name"], "crucible-staging");
        assert_eq!(doc["accessTokenMaxAgeSeconds"], 120);
    }

    #[test]
    fn a_cluster_the_table_does_not_list_or_the_fleet_does_not_know_is_refused() {
        let p = profile(PROFILE);
        let err = render(&p, "east", None).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ClusterLinkError>(),
            Some(&ClusterLinkError::NotListed {
                cluster: "east".into(),
                listed: vec!["ocp".into(), "hub".into(), "gone".into(), "k8s".into()]
            })
        );
        let err = render(&p, "gone", None).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ClusterLinkError>(),
            Some(&ClusterLinkError::UnknownCluster {
                cluster: "gone".into()
            })
        );
    }

    #[test]
    fn a_cluster_whose_usernames_follow_the_sso_login_gets_no_oauth_client() {
        let err = render(&profile(PROFILE), "k8s", None).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ClusterLinkError>(),
            Some(&ClusterLinkError::NotLinkCluster {
                cluster: "k8s".into()
            })
        );
    }

    #[test]
    fn a_profile_without_the_table_or_with_a_plain_http_url_is_refused() {
        let bare = PROFILE.split("[cluster_link]").next().unwrap().to_string();
        let err = render(&profile(&bare), "ocp", None).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ClusterLinkError>(),
            Some(&ClusterLinkError::MissingTable)
        );
        let http = PROFILE.replace("https://crucible", "http://crucible");
        let err = format!("{:#}", render(&profile(&http), "ocp", None).unwrap_err());
        assert!(err.contains("must be https"), "{err}");
    }

    #[test]
    fn without_an_external_url_the_callback_sits_under_the_agent_issuer() {
        let text = PROFILE.replace(
            "external_url = \"https://crucible.apps.hub.example.com\"\n",
            "",
        );
        let err = render(&profile(&text), "ocp", None).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ClusterLinkError>(),
            Some(&ClusterLinkError::NoBrowserUrl)
        );
        let doc: serde_json::Value = serde_norway::from_str(
            &render(&profile(&text), "ocp", Some("https://crucible.example.com")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            doc["redirectURIs"][0],
            "https://crucible.example.com/oauth/cluster-link/ocp/callback"
        );
        let doc: serde_json::Value = serde_norway::from_str(
            &render(
                &profile(PROFILE),
                "ocp",
                Some("https://crucible.example.com"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            doc["redirectURIs"][0],
            "https://crucible.apps.hub.example.com/oauth/cluster-link/ocp/callback",
            "a stated external_url wins"
        );
    }

    #[test]
    fn a_cluster_name_that_is_not_a_label_fails_to_parse() {
        let text = PROFILE.replace("\"gone\"", "\"../gone\"");
        assert!(toml::from_str::<DeployProfile>(&text).is_err());
    }
}

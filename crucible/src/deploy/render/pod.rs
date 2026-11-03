//! The profile-derived shape every rendered pod shares: the pull secret, the node avoid-list,
//! the istio opt-out, and the service account, so each renderer supplies only its containers,
//! init containers, and volumes.

use crate::deploy::profile::DeployProfile;
use crate::deploy::render::kube::node_avoid_affinity;
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::deploy) enum RestartPolicy {
    Never,
    OnFailure,
}

impl RestartPolicy {
    fn as_str(self) -> &'static str {
        match self {
            RestartPolicy::Never => "Never",
            RestartPolicy::OnFailure => "OnFailure",
        }
    }
}

/// What a pod rendered from a [`DeployProfile`] states about itself before its containers.
pub(in crate::deploy) struct ProfilePod<'a> {
    pub profile: &'a DeployProfile,
    pub labels: BTreeMap<String, String>,
    pub service_account: String,
    pub automount_token: Option<bool>,
    /// `None` leaves the policy to the owner (a Deployment's template).
    pub restart: Option<RestartPolicy>,
    pub host_aliases: Option<Vec<core::HostAlias>>,
}

impl ProfilePod<'_> {
    /// A standalone pod named `name` in `namespace`.
    pub fn pod(
        self,
        name: String,
        namespace: String,
        containers: Vec<core::Container>,
        init: Option<Vec<core::Container>>,
        volumes: Option<Vec<core::Volume>>,
    ) -> core::Pod {
        core::Pod {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(namespace),
                ..self.metadata()
            },
            spec: Some(self.spec(containers, init, volumes)),
            ..Default::default()
        }
    }

    /// The pod as an owner's template.
    pub fn template(
        self,
        containers: Vec<core::Container>,
        init: Option<Vec<core::Container>>,
        volumes: Option<Vec<core::Volume>>,
    ) -> core::PodTemplateSpec {
        core::PodTemplateSpec {
            metadata: Some(self.metadata()),
            spec: Some(self.spec(containers, init, volumes)),
        }
    }

    fn metadata(&self) -> ObjectMeta {
        ObjectMeta {
            annotations: Some(BTreeMap::from([(
                "sidecar.istio.io/inject".to_string(),
                "false".to_string(),
            )])),
            labels: Some(self.labels.clone()),
            ..Default::default()
        }
    }

    fn spec(
        &self,
        containers: Vec<core::Container>,
        init: Option<Vec<core::Container>>,
        volumes: Option<Vec<core::Volume>>,
    ) -> core::PodSpec {
        core::PodSpec {
            image_pull_secrets: Some(vec![core::LocalObjectReference {
                name: self.profile.image.pull_secret.clone(),
            }]),
            service_account_name: Some(self.service_account.clone()),
            automount_service_account_token: self.automount_token,
            restart_policy: self.restart.map(|r| r.as_str().to_string()),
            affinity: node_avoid_affinity(self.profile),
            host_aliases: self.host_aliases.clone(),
            init_containers: init,
            containers,
            volumes,
            ..Default::default()
        }
    }
}

/// A literal env var.
pub(in crate::deploy) fn env_var(name: &str, value: impl Into<String>) -> core::EnvVar {
    core::EnvVar {
        name: name.to_string(),
        value: Some(value.into()),
        value_from: None,
    }
}

/// An env var read off the pod's own object through the downward API.
pub(in crate::deploy) fn downward(name: &str, field_path: &str) -> core::EnvVar {
    core::EnvVar {
        name: name.to_string(),
        value: None,
        value_from: Some(core::EnvVarSource {
            field_ref: Some(core::ObjectFieldSelector {
                field_path: field_path.to_string(),
                api_version: None,
            }),
            ..Default::default()
        }),
    }
}

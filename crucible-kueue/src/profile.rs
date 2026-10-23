//! The manifest allowlist as data. `strict` is crucible's: nothing that reaches a Secret, a
//! volume claim, a toleration or a Kueue setting of the agent's choosing. `tenant` is for callers
//! whose own RBAC already covers those: it adds them and reserves only the server's label domain
//! and the keys it sets. A TOML file can start from either and add or remove entries; a CEL tool
//! policy can tighten any of them further.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

/// Entries of each allowlist. Field names are the JSON field names of the object they apply to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lists {
    /// Pod spec fields.
    pub pod: BTreeSet<String>,
    /// Container fields.
    pub container: BTreeSet<String>,
    /// Container `resources` fields.
    pub resources: BTreeSet<String>,
    /// Volume sources.
    pub volume_sources: BTreeSet<String>,
    /// Projected volume sources.
    pub projected_sources: BTreeSet<String>,
    /// `env[].valueFrom` sources.
    pub env_sources: BTreeSet<String>,
    /// `envFrom[]` fields.
    pub env_from: BTreeSet<String>,
    /// Label and annotation domains reserved for the server and the platform, besides the
    /// server's own label domain.
    pub reserved_domains: BTreeSet<String>,
    /// Labels inside a reserved domain a manifest may still set.
    pub allowed_labels: BTreeSet<String>,
    /// Annotations inside a reserved domain a manifest may still set.
    pub allowed_annotations: BTreeSet<String>,
}

impl Lists {
    fn add(&mut self, other: &Lists) {
        for (mine, theirs) in self.each_mut().into_iter().zip(other.each()) {
            mine.extend(theirs.iter().cloned());
        }
    }

    fn remove(&mut self, other: &Lists) {
        for (mine, theirs) in self.each_mut().into_iter().zip(other.each()) {
            mine.retain(|e| !theirs.contains(e));
        }
    }

    fn each(&self) -> [&BTreeSet<String>; 10] {
        [
            &self.pod,
            &self.container,
            &self.resources,
            &self.volume_sources,
            &self.projected_sources,
            &self.env_sources,
            &self.env_from,
            &self.reserved_domains,
            &self.allowed_labels,
            &self.allowed_annotations,
        ]
    }

    fn each_mut(&mut self) -> [&mut BTreeSet<String>; 10] {
        [
            &mut self.pod,
            &mut self.container,
            &mut self.resources,
            &mut self.volume_sources,
            &mut self.projected_sources,
            &mut self.env_sources,
            &mut self.env_from,
            &mut self.reserved_domains,
            &mut self.allowed_labels,
            &mut self.allowed_annotations,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub lists: Lists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuiltIn {
    Strict,
    Tenant,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    base: BuiltIn,
    #[serde(default)]
    add: Lists,
    #[serde(default)]
    remove: Lists,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("reading allowlist profile {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing allowlist profile {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: Box<toml::de::Error>,
    },
}

fn set(entries: &[&str]) -> BTreeSet<String> {
    entries.iter().map(|s| s.to_string()).collect()
}

const STRICT_POD: &[&str] = &[
    "containers",
    "initContainers",
    "restartPolicy",
    "terminationGracePeriodSeconds",
    "activeDeadlineSeconds",
    "dnsPolicy",
    "dnsConfig",
    "nodeSelector",
    "affinity",
    "topologySpreadConstraints",
    "securityContext",
    "volumes",
    "imagePullSecrets",
    "hostname",
    "subdomain",
    "setHostnameAsFQDN",
    "enableServiceLinks",
    "shareProcessNamespace",
    "hostAliases",
    "readinessGates",
    "automountServiceAccountToken",
];
const CONTAINER: &[&str] = &[
    "name",
    "image",
    "imagePullPolicy",
    "command",
    "args",
    "workingDir",
    "env",
    "envFrom",
    "ports",
    "resources",
    "volumeMounts",
    "livenessProbe",
    "readinessProbe",
    "startupProbe",
    "lifecycle",
    "terminationMessagePath",
    "terminationMessagePolicy",
    "stdin",
    "stdinOnce",
    "tty",
    "securityContext",
    "restartPolicy",
    "resizePolicy",
];
const STRICT_VOLUME_SOURCES: &[&str] = &[
    "emptyDir",
    "configMap",
    "downwardAPI",
    "ephemeral",
    "projected",
];

/// Kueue settings a tenant may choose: its priority class and Topology Aware Scheduling.
pub const TENANT_KUEUE_LABELS: &[&str] = &["kueue.x-k8s.io/priority-class"];
pub const TENANT_KUEUE_ANNOTATIONS: &[&str] = &[
    "kueue.x-k8s.io/podset-required-topology",
    "kueue.x-k8s.io/podset-preferred-topology",
];

impl Profile {
    pub fn strict() -> Self {
        Self {
            name: "strict".into(),
            lists: Lists {
                pod: set(STRICT_POD),
                container: set(CONTAINER),
                resources: set(&["requests", "limits"]),
                volume_sources: set(STRICT_VOLUME_SOURCES),
                projected_sources: set(&["configMap", "downwardAPI"]),
                env_sources: set(&["fieldRef", "resourceFieldRef", "configMapKeyRef"]),
                env_from: set(&["configMapRef", "prefix"]),
                reserved_domains: set(&["kueue.x-k8s.io", "openshell.ai", "spiffe.io"]),
                allowed_labels: BTreeSet::new(),
                allowed_annotations: BTreeSet::new(),
            },
        }
    }

    pub fn tenant() -> Self {
        let mut lists = Self::strict().lists;
        lists.add(&Lists {
            pod: set(&["tolerations", "priorityClassName", "resourceClaims"]),
            resources: set(&["claims"]),
            volume_sources: set(&["secret", "persistentVolumeClaim"]),
            projected_sources: set(&["secret"]),
            env_sources: set(&["secretKeyRef"]),
            env_from: set(&["secretRef"]),
            allowed_labels: set(TENANT_KUEUE_LABELS),
            allowed_annotations: set(TENANT_KUEUE_ANNOTATIONS),
            ..Lists::default()
        });
        lists.reserved_domains.clear();
        Self {
            name: "tenant".into(),
            lists,
        }
    }

    pub fn built_in(b: BuiltIn) -> Self {
        match b {
            BuiltIn::Strict => Self::strict(),
            BuiltIn::Tenant => Self::tenant(),
        }
    }

    /// `strict`, `tenant`, or a TOML file: `base = "strict"|"tenant"` with optional `[add]` and
    /// `[remove]` tables of [`Lists`].
    pub fn load(spec: &str) -> Result<Self, ProfileError> {
        match spec {
            "strict" => return Ok(Self::strict()),
            "tenant" => return Ok(Self::tenant()),
            _ => {}
        }
        let text =
            std::fs::read_to_string(Path::new(spec)).map_err(|source| ProfileError::Read {
                path: spec.to_string(),
                source,
            })?;
        Self::from_toml(spec, &text)
    }

    pub fn from_toml(name: &str, text: &str) -> Result<Self, ProfileError> {
        let file: ProfileFile = toml::from_str(text).map_err(|source| ProfileError::Parse {
            path: name.to_string(),
            source: Box::new(source),
        })?;
        let mut profile = Self::built_in(file.base);
        profile.lists.add(&file.add);
        profile.lists.remove(&file.remove);
        profile.name = name.to_string();
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use crate::profile::*;

    #[test]
    fn strict_is_the_allowlist_crucible_shipped() {
        let s = Profile::strict().lists;
        assert!(s.pod.contains("nodeSelector"));
        for refused in [
            "tolerations",
            "priorityClassName",
            "resourceClaims",
            "serviceAccountName",
            "hostNetwork",
            "nodeName",
        ] {
            assert!(!s.pod.contains(refused), "{refused}");
        }
        assert!(!s.volume_sources.contains("secret"));
        assert!(!s.env_sources.contains("secretKeyRef"));
        assert!(s.reserved_domains.contains("kueue.x-k8s.io"));
    }

    #[test]
    fn tenant_adds_secrets_claims_tolerations_and_kueue_choices_but_no_host_access() {
        let t = Profile::tenant().lists;
        for added in [
            "tolerations",
            "priorityClassName",
            "resourceClaims",
            "imagePullSecrets",
        ] {
            assert!(t.pod.contains(added), "{added}");
        }
        for refused in [
            "serviceAccountName",
            "hostNetwork",
            "hostPID",
            "nodeName",
            "schedulerName",
            "runtimeClassName",
        ] {
            assert!(!t.pod.contains(refused), "{refused}");
        }
        assert!(
            t.volume_sources.contains("secret")
                && t.volume_sources.contains("persistentVolumeClaim")
        );
        assert!(!t.volume_sources.contains("hostPath"));
        assert!(t.env_sources.contains("secretKeyRef") && t.env_from.contains("secretRef"));
        assert!(t.resources.contains("claims"));
        assert!(t.reserved_domains.is_empty());
        assert!(t.allowed_labels.contains("kueue.x-k8s.io/priority-class"));
        assert!(
            t.allowed_annotations
                .contains("kueue.x-k8s.io/podset-required-topology")
        );
    }

    #[test]
    fn a_file_starts_from_a_built_in_and_adds_or_removes() {
        let p = Profile::from_toml(
            "site.toml",
            r#"
                base = "tenant"
                [add]
                pod = ["runtimeClassName"]
                [remove]
                volume_sources = ["persistentVolumeClaim"]
                env_sources = ["secretKeyRef"]
            "#,
        )
        .unwrap();
        assert_eq!(p.name, "site.toml");
        assert!(p.lists.pod.contains("runtimeClassName"));
        assert!(!p.lists.volume_sources.contains("persistentVolumeClaim"));
        assert!(p.lists.volume_sources.contains("secret"));
        assert!(!p.lists.env_sources.contains("secretKeyRef"));
        assert!(Profile::from_toml("x", "base = \"loose\"").is_err());
        assert!(Profile::from_toml("x", "base = \"strict\"\n[add]\nhost = [\"x\"]").is_err());
        assert_eq!(Profile::load("strict").unwrap(), Profile::strict());
        assert!(matches!(
            Profile::load("/no/such/profile.toml"),
            Err(ProfileError::Read { .. })
        ));
    }
}

use crate::manifest::secret::{SecretDecl, SecretKind};
use serde::Deserialize;

/// `[buildit]`: where this pack's runs push their builds, over the deployment's own push
/// repository. The engine never reads it; the controller starts the run's buildit with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilditPushCfg {
    /// The one repository `push` publishes builds to.
    #[serde(default)]
    pub push_repo: Option<Repository>,
    /// The `[[secret]]` of kind `registry_authfile` that pushes to it.
    #[serde(default)]
    pub push_secret: Option<String>,
}

/// `[kueue]`: the registry credentials this pack's workloads pull with, beside the deployment's
/// own pull secrets. The engine never reads it; the controller writes each run's into the
/// workload namespaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KueuePullCfg {
    #[serde(default)]
    pub pull_secrets: Vec<PullSecretCfg>,
}

/// One pull credential: a declared secret and the images it pulls.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullSecretCfg {
    /// The `[[secret]]` of kind `registry_authfile` that holds the credential.
    pub secret: String,
    /// The registry host, or host/path prefix, whose images pull with it.
    pub registry: Repository,
}

/// A registry host with an optional repository path, `quay.io` or `quay.io/team/app`: lowercase,
/// no scheme, tag or digest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Repository(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0:?} is not a registry host or <registry>/<path>, lowercase, without a scheme or tag")]
pub struct RepositoryError(String);

impl TryFrom<String> for Repository {
    type Error = RepositoryError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let name = raw.trim().trim_end_matches('/');
        let (host, path) = name.split_once('/').unwrap_or((name, ""));
        let valid = !host.is_empty()
            && !name.contains("//")
            && !path.contains(':')
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-/:".contains(&b));
        match valid {
            true => Ok(Repository(name.to_string())),
            false => Err(RepositoryError(raw)),
        }
    }
}

impl Repository {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn has_path(&self) -> bool {
        self.0.contains('/')
    }
}

impl std::fmt::Display for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    #[error("[buildit] push_repo and push_secret go together; this pack names only {named}")]
    HalfAPush { named: &'static str },
    #[error("[buildit] push_repo {0:?} names a registry and no repository in it")]
    PushRepoIsAHost(String),
    #[error("{table} names secret {name:?}, which no [[secret]] declares")]
    Undeclared { table: &'static str, name: String },
    #[error(
        "{table} names secret {name:?}, which is kind {kind:?}; a registry credential is kind \
         \"registry_authfile\""
    )]
    NotAnAuthfile {
        table: &'static str,
        name: String,
        kind: &'static str,
    },
    #[error("[kueue] pull_secrets names registry {0:?} twice")]
    RegistryTwice(String),
}

fn check_authfile(
    table: &'static str,
    name: &str,
    secrets: &[SecretDecl],
) -> Result<(), RegistryError> {
    let declared = secrets
        .iter()
        .find(|decl| decl.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| RegistryError::Undeclared {
            table,
            name: name.to_string(),
        })?;
    match declared.kind {
        SecretKind::RegistryAuthfile => Ok(()),
        kind => Err(RegistryError::NotAnAuthfile {
            table,
            name: name.to_string(),
            kind: kind.as_str(),
        }),
    }
}

/// The cross-table checks on `[buildit]` and `[kueue]`.
pub fn validate_registries(
    buildit: &BuilditPushCfg,
    kueue: &KueuePullCfg,
    secrets: &[SecretDecl],
) -> Result<(), RegistryError> {
    match (&buildit.push_repo, &buildit.push_secret) {
        (None, None) => {}
        (Some(_), None) => return Err(RegistryError::HalfAPush { named: "push_repo" }),
        (None, Some(_)) => {
            return Err(RegistryError::HalfAPush {
                named: "push_secret",
            });
        }
        (Some(repo), Some(secret)) => {
            if !repo.has_path() {
                return Err(RegistryError::PushRepoIsAHost(repo.to_string()));
            }
            check_authfile("[buildit] push_secret", secret, secrets)?;
        }
    }
    let mut seen: Vec<&str> = Vec::with_capacity(kueue.pull_secrets.len());
    for pull in &kueue.pull_secrets {
        check_authfile("[kueue] pull_secrets", &pull.secret, secrets)?;
        if seen.contains(&pull.registry.as_str()) {
            return Err(RegistryError::RegistryTwice(pull.registry.to_string()));
        }
        seen.push(pull.registry.as_str());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;
    use crate::manifest::registry::*;

    const AUTHFILE: &str = r#"
        [[secret]]
        name = "registry"
        kind = "registry_authfile"
        path = "/etc/registry/auth.json"
    "#;

    fn parse(extra: &str) -> anyhow::Result<Manifest> {
        let m: Manifest = toml::from_str(&format!(
            r#"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [workflow]
            type = "playbook"
            file = "workflow.star"
            {extra}
            "#
        ))?;
        validate_registries(&m.buildit, &m.kueue, &m.secrets)?;
        Ok(m)
    }

    fn refused(extra: &str) -> String {
        parse(extra).err().expect("refused").to_string()
    }

    #[test]
    fn a_manifest_without_the_tables_brings_no_registry_credentials() {
        let m = parse("").unwrap();
        assert_eq!(m.buildit, BuilditPushCfg::default());
        assert_eq!(m.kueue, KueuePullCfg::default());
    }

    #[test]
    fn a_pack_names_its_push_repository_and_the_secrets_that_push_and_pull() {
        let m = parse(&format!(
            "{AUTHFILE}
            [buildit]
            push_repo = \"quay.io/team/smoke/\"
            push_secret = \"Registry\"
            [kueue]
            pull_secrets = [{{ secret = \"registry\", registry = \"quay.io/team\" }}]"
        ))
        .unwrap();
        assert_eq!(
            m.buildit.push_repo.as_ref().map(Repository::as_str),
            Some("quay.io/team/smoke")
        );
        assert_eq!(m.buildit.push_secret.as_deref(), Some("Registry"));
        assert_eq!(m.kueue.pull_secrets[0].secret, "registry");
        assert_eq!(m.kueue.pull_secrets[0].registry.as_str(), "quay.io/team");
    }

    #[test]
    fn a_push_needs_both_its_repository_and_its_secret() {
        let only_repo = refused("[buildit]\npush_repo = \"quay.io/team/smoke\"");
        assert!(only_repo.contains("names only push_repo"), "{only_repo}");
        let only_secret = refused(&format!("{AUTHFILE}[buildit]\npush_secret = \"registry\""));
        assert!(
            only_secret.contains("names only push_secret"),
            "{only_secret}"
        );
    }

    #[test]
    fn a_push_repository_is_a_path_in_a_registry() {
        let err = refused(&format!(
            "{AUTHFILE}[buildit]\npush_repo = \"quay.io\"\npush_secret = \"registry\""
        ));
        assert!(err.contains("no repository in it"), "{err}");
    }

    #[test]
    fn a_registry_credential_is_a_declared_authfile() {
        let undeclared =
            refused("[buildit]\npush_repo = \"quay.io/team/smoke\"\npush_secret = \"registry\"");
        assert!(
            undeclared.contains("which no [[secret]] declares"),
            "{undeclared}"
        );
        let opaque = refused(
            "[[secret]]\nname = \"token\"\n[kueue]\npull_secrets = [{ secret = \"token\", registry = \"quay.io\" }]",
        );
        assert!(opaque.contains("which is kind \"opaque\""), "{opaque}");
    }

    #[test]
    fn a_registry_pulls_with_one_secret() {
        let err = refused(&format!(
            "{AUTHFILE}[kueue]\npull_secrets = [
                {{ secret = \"registry\", registry = \"quay.io/team\" }},
                {{ secret = \"registry\", registry = \"quay.io/team/\" }},
            ]"
        ));
        assert!(err.contains("\"quay.io/team\" twice"), "{err}");
    }

    #[test]
    fn a_repository_has_no_scheme_tag_digest_or_uppercase() {
        for bad in [
            "",
            "https://quay.io/team",
            "quay.io/team:latest",
            "quay.io/team@sha256:abc",
            "quay.io/Team",
            "quay.io//team",
            "/team",
            "quay.io/team app",
        ] {
            assert_eq!(
                Repository::try_from(bad.to_string()),
                Err(RepositoryError(bad.to_string())),
                "{bad}"
            );
        }
        for good in ["quay.io", "registry.local:5000/org/smoke", "quay.io/team"] {
            assert_eq!(
                Repository::try_from(good.to_string()).unwrap().as_str(),
                good
            );
        }
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(parse("[buildit]\nrepo = \"quay.io/team/smoke\"").is_err());
        assert!(parse("[kueue]\npull_secret = \"registry\"").is_err());
    }
}

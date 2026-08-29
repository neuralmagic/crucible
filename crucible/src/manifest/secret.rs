use serde::Deserialize;

/// The longest secret name the registry stores.
const MAX_NAME: usize = 96;

/// Why a `[[secret]]` block is unusable. Checked at manifest load, before a run is dispatched.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SecretError {
    #[error("[[secret]].name must be 1..={MAX_NAME} characters of [A-Za-z0-9._-], got {got:?}")]
    BadName { got: String },
    #[error("[[secret]] {name:?} names both an env var and a file path; it is one or the other")]
    ProjectionConflict { name: String },
    #[error(
        "[[secret]] {name:?} is kind {kind:?}, which projects to a file: only an opaque secret \
         can take an env projection"
    )]
    EnvNeedsOpaque { name: String, kind: String },
    #[error("[[secret]] {name:?} is declared more than once (names match case-insensitively)")]
    Duplicate { name: String },
}

/// What a redeemed value is, which decides how it may be projected.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    /// An opaque string. The only kind that may reach a run as an environment variable.
    #[default]
    Opaque,
    File,
    RegistryAuthfile,
    Kubeconfig,
}

impl SecretKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Opaque => "opaque",
            Self::File => "file",
            Self::RegistryAuthfile => "registry_authfile",
            Self::Kubeconfig => "kubeconfig",
        }
    }
}

/// One secret a pack declares it needs. The declaration names it and says how the run wants it
/// projected; the value, its visibility, and whether this launcher may have it at all belong to
/// the registry that binds the name, never to the pack.
///
/// Both projections absent leaves the choice to the binding.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct SecretDecl {
    pub name: String,
    #[serde(default)]
    pub kind: SecretKind,
    /// Project the value into this environment variable. Opaque secrets only.
    #[serde(default)]
    pub env: Option<String>,
    /// Write the value to this path instead.
    #[serde(default)]
    pub path: Option<String>,
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The rules the registry enforces on a binding, checked here so a pack that could never be
/// bound is refused where it is authored rather than at dispatch.
pub fn validate_secrets(secrets: &[SecretDecl]) -> Result<(), SecretError> {
    let mut seen: Vec<String> = Vec::with_capacity(secrets.len());
    for secret in secrets {
        if !name_ok(&secret.name) {
            return Err(SecretError::BadName {
                got: secret.name.clone(),
            });
        }
        if secret.env.is_some() && secret.path.is_some() {
            return Err(SecretError::ProjectionConflict {
                name: secret.name.clone(),
            });
        }
        if secret.env.is_some() && secret.kind != SecretKind::Opaque {
            return Err(SecretError::EnvNeedsOpaque {
                name: secret.name.clone(),
                kind: secret.kind.as_str().to_string(),
            });
        }
        let folded = secret.name.to_ascii_lowercase();
        if seen.contains(&folded) {
            return Err(SecretError::Duplicate {
                name: secret.name.clone(),
            });
        }
        seen.push(folded);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(name: &str, kind: SecretKind, env: Option<&str>, path: Option<&str>) -> SecretDecl {
        SecretDecl {
            name: name.to_string(),
            kind,
            env: env.map(str::to_string),
            path: path.map(str::to_string),
        }
    }

    #[test]
    fn accepts_the_shapes_the_registry_binds() {
        let secrets = vec![
            decl("pr_token", SecretKind::Opaque, Some("GH_TOKEN"), None),
            decl(
                "registry",
                SecretKind::RegistryAuthfile,
                None,
                Some("/etc/quay/push.json"),
            ),
            decl("left-to-the-binding", SecretKind::Opaque, None, None),
        ];
        assert_eq!(validate_secrets(&secrets), Ok(()));
    }

    #[test]
    fn kind_defaults_to_opaque_and_parses_snake_case() {
        let parsed: SecretDecl = toml::from_str("name = \"pr_token\"").unwrap();
        assert_eq!(parsed.kind, SecretKind::Opaque);
        let parsed: SecretDecl =
            toml::from_str("name = \"reg\"\nkind = \"registry_authfile\"").unwrap();
        assert_eq!(parsed.kind, SecretKind::RegistryAuthfile);
    }

    #[test]
    fn rejects_a_name_the_registry_could_not_store() {
        for bad in ["", "has space", "slash/es", &"x".repeat(MAX_NAME + 1)] {
            assert!(
                matches!(
                    validate_secrets(&[decl(bad, SecretKind::Opaque, None, None)]),
                    Err(SecretError::BadName { .. })
                ),
                "expected {bad:?} to be refused"
            );
        }
    }

    #[test]
    fn a_name_the_controller_accepts_is_not_refused_here() {
        // The registry folds case when it matches a binding, so core must not be the stricter
        // reader: an uppercase name that binds there has to parse here too.
        assert_eq!(
            validate_secrets(&[decl("PR_Token", SecretKind::Opaque, None, None)]),
            Ok(())
        );
    }

    #[test]
    fn rejects_both_projections() {
        assert_eq!(
            validate_secrets(&[decl(
                "pr_token",
                SecretKind::Opaque,
                Some("GH_TOKEN"),
                Some("/tmp/token")
            )]),
            Err(SecretError::ProjectionConflict {
                name: "pr_token".to_string()
            })
        );
    }

    #[test]
    fn rejects_an_env_projection_for_a_file_kind() {
        assert_eq!(
            validate_secrets(&[decl(
                "kubeconfig",
                SecretKind::Kubeconfig,
                Some("KUBECONFIG"),
                None
            )]),
            Err(SecretError::EnvNeedsOpaque {
                name: "kubeconfig".to_string(),
                kind: "kubeconfig".to_string()
            })
        );
    }

    #[test]
    fn rejects_a_duplicate_however_it_is_cased() {
        assert_eq!(
            validate_secrets(&[
                decl("pr_token", SecretKind::Opaque, None, None),
                decl("PR_TOKEN", SecretKind::Opaque, None, None),
            ]),
            Err(SecretError::Duplicate {
                name: "PR_TOKEN".to_string()
            })
        );
    }

    #[test]
    fn rejects_an_unknown_field() {
        let bad: Result<SecretDecl, _> = toml::from_str("name = \"pr\"\nvisibility = \"agent\"");
        assert!(
            bad.is_err(),
            "visibility belongs to the binding, not the pack"
        );
    }
}

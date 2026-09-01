//! `[[capabilities.secret]]`: the pack's declaration of the credentials a run holds.
//!
//! Egress, relay files, and a substituted broker binary are already declared elsewhere in the
//! manifest and are read from there, so the only thing this table adds is what a credential
//! *authorizes*: a name alone does not state reach.

use serde::Deserialize;

/// Whether a credential's value enters the agent's context or stays broker-held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialContext {
    /// The value is delivered into the sandbox, so the agent can read it.
    Agent,
    /// The value stays on the loop pod; the agent reaches it only through a mediated tool.
    Broker,
}

impl CredentialContext {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialContext::Agent => "agent",
            CredentialContext::Broker => "broker",
        }
    }
}

impl std::fmt::Display for CredentialContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One declared credential.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretDecl {
    /// The environment variable (or relay destination) the value arrives as.
    pub name: String,
    /// Where the value lives.
    pub context: CredentialContext,
    /// The external system the credential authorizes against, e.g. `jira` or `quay.io`.
    pub system: String,
    /// What it authorizes there, e.g. `comment on PROJ` or `push to aipcc/*`.
    pub scope: String,
}

/// The `[capabilities]` table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesCfg {
    #[serde(default)]
    pub secret: Vec<SecretDecl>,
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilitiesError {
    #[error("[[capabilities.secret]] entry {index} has an empty `{field}`")]
    EmptyField { index: usize, field: &'static str },
    #[error("[[capabilities.secret]] declares `{name}` twice")]
    Duplicate { name: String },
}

pub fn validate_capabilities(cfg: &CapabilitiesCfg) -> anyhow::Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for (index, s) in cfg.secret.iter().enumerate() {
        for (field, value) in [
            ("name", &s.name),
            ("system", &s.system),
            ("scope", &s.scope),
        ] {
            if value.trim().is_empty() {
                return Err(CapabilitiesError::EmptyField { index, field }.into());
            }
        }
        if seen.contains(&s.name.as_str()) {
            return Err(CapabilitiesError::Duplicate {
                name: s.name.clone(),
            }
            .into());
        }
        seen.push(&s.name);
    }
    Ok(())
}

impl CapabilitiesCfg {
    /// The declaration covering `name`, if any.
    pub fn secret_named(&self, name: &str) -> Option<&SecretDecl> {
        self.secret.iter().find(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    const BASE: &str = r#"
        [repo]
        path = "."
        [agent]
        backend = "openshell"
        goal = "g"
        [judge]
        measure_cmd = "m"
        direction = "higher"
    "#;

    fn load(extra: &str) -> anyhow::Result<Manifest> {
        let m: Manifest = toml::from_str(&format!("{BASE}{extra}"))?;
        m.validate()?;
        Ok(m)
    }

    fn parse(extra: &str) -> Manifest {
        match load(extra) {
            Ok(m) => m,
            Err(e) => panic!("expected {extra:?} to parse: {e:#}"),
        }
    }

    fn refusal(extra: &str) -> String {
        match load(extra) {
            Ok(_) => panic!("expected {extra:?} to be refused"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn a_declared_secret_parses_with_its_reach() {
        let m = parse(
            r#"
            [[capabilities.secret]]
            name = "JIRA_API_TOKEN"
            context = "broker"
            system = "jira"
            scope = "read + comment on PROJ"
        "#,
        );
        let decl = m
            .capabilities
            .secret_named("JIRA_API_TOKEN")
            .expect("declared");
        assert_eq!(decl.context, CredentialContext::Broker);
        assert_eq!(decl.system, "jira");
    }

    #[test]
    fn an_empty_field_or_a_duplicate_name_is_a_manifest_error() {
        let err = refusal(
            "[[capabilities.secret]]\nname = \"\"\ncontext = \"agent\"\nsystem = \"s\"\nscope = \"x\"\n",
        );
        assert!(err.contains("empty `name`"), "{err}");
        let one = "[[capabilities.secret]]\nname = \"T\"\ncontext = \"agent\"\nsystem = \"s\"\nscope = \"x\"\n";
        let err = refusal(&format!("{one}{one}"));
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn an_unknown_context_is_a_manifest_error() {
        let err = refusal(
            "[[capabilities.secret]]\nname = \"T\"\ncontext = \"root\"\nsystem = \"s\"\nscope = \"x\"\n",
        );
        assert!(err.contains("root"), "{err}");
    }
}

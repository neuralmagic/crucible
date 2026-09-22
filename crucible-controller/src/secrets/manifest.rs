//! What a pack says it needs, and what the deploy profile already supplies.
//!
//! A pack declares its credentials in `crucible.toml` the way it declares its builds:
//!
//! ```toml
//! [[secret]]
//! name = "pr_token"
//! kind = "opaque"
//! env  = "AUTORESEARCH_PR_TOKEN"
//!
//! [[secret]]
//! name = "registry"
//! kind = "registry_authfile"
//! path = "/etc/quay/push.json"
//! ```
//!
//! A binding is the only mapping from a declared name to a value; the declaration carries the
//! projection the pack expects, so the bind form can prefill it. Unknown keys are ignored, because
//! the engine owns this block and may grow it.
//!
//! The deploy profile's `[[secret_env]]` is the pre-registry path: a Kubernetes Secret an admin
//! placed, named in the profile. It stays authoritative for a pack until the pack's manifest
//! declares secrets, so a name that appears in both is what the preview gate warns about.

#![allow(clippy::disallowed_macros)]

use crate::secrets::{ProjectionKind, SecretKind, SecretName};
use anyhow::{Context, Result};
use std::path::Path;

/// One credential a pack declares.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeclaredSecret {
    pub name: SecretName,
    pub kind: SecretKind,
    /// How the pack expects the value: the env var or the path it reads. `None` when the manifest
    /// declared a name and left the projection to the binding.
    pub projection: Option<(ProjectionKind, String)>,
}

#[derive(serde::Deserialize)]
struct ManifestSecrets {
    #[serde(default)]
    secret: Vec<SecretEntry>,
}

#[derive(serde::Deserialize)]
struct SecretEntry {
    name: String,
    kind: Option<String>,
    env: Option<String>,
    path: Option<String>,
}

/// Read the declared secrets out of a pack tree's manifest. A tree with no manifest, or a manifest
/// with no `[[secret]]`, declares none; a malformed entry is an error, because a pack that names a
/// credential the registry cannot resolve must not launch quietly without it.
pub fn declared_secrets(pack_root: &Path) -> Result<Vec<DeclaredSecret>> {
    let manifest = pack_root.join("crucible.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Ok(Vec::new());
    };
    parse_declared(&text).with_context(|| format!("reading the secrets block of {manifest:?}"))
}

/// The parse half, over manifest text.
pub fn parse_declared(text: &str) -> Result<Vec<DeclaredSecret>> {
    let parsed: ManifestSecrets = toml::from_str(text).context("parsing the pack manifest")?;
    parsed
        .secret
        .into_iter()
        .map(|entry| {
            let name = SecretName::parse(&entry.name)
                .with_context(|| format!("the declared secret {:?}", entry.name))?;
            let kind = match entry.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
                Some(k) => SecretKind::parse(k)
                    .with_context(|| format!("the declared secret {name:?}"))?,
                None => SecretKind::Opaque,
            };
            let projection = match (nonempty(entry.env), nonempty(entry.path)) {
                (Some(_), Some(_)) => anyhow::bail!(
                    "the declared secret {name:?} names both an env var and a file path; it is one or the other"
                ),
                (Some(env), None) => Some((ProjectionKind::Env, env)),
                (None, Some(path)) => Some((ProjectionKind::File, path)),
                (None, None) => None,
            };
            Ok(DeclaredSecret {
                name,
                kind,
                projection,
            })
        })
        .collect()
}

fn nonempty(raw: Option<String>) -> Option<String> {
    raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[derive(serde::Deserialize)]
struct ProfileSecretEnv {
    #[serde(default)]
    secret_env: Vec<SecretEnvEntry>,
}

#[derive(serde::Deserialize)]
struct SecretEnvEntry {
    name: String,
}

/// The environment variable names the deploy profile already fills from pre-created Kubernetes
/// Secrets. Read once at startup; a profile that does not parse contributes no names rather than
/// failing the boot, since every other consumer of the profile is the core render.
pub fn profile_secret_env(profile: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(profile) else {
        return Vec::new();
    };
    parse_secret_env(&text)
}

/// The parse half, over profile text.
pub fn parse_secret_env(text: &str) -> Vec<String> {
    toml::from_str::<ProfileSecretEnv>(text)
        .map(|p| {
            p.secret_env
                .into_iter()
                .filter_map(|e| nonempty(Some(e.name)))
                .collect()
        })
        .unwrap_or_default()
}

/// The declared names that the deploy profile also fills. Both sides would write the same
/// environment variable, so the gate says which one wins: the binding.
pub fn conflicting_names(declared: &[DeclaredSecret], profile_env: &[String]) -> Vec<String> {
    let mut hits: Vec<String> = declared
        .iter()
        .filter(|d| {
            let name = d.name.as_str();
            let projected = d
                .projection
                .as_ref()
                .and_then(|(kind, value)| (*kind == ProjectionKind::Env).then_some(value.as_str()));
            profile_env
                .iter()
                .any(|env| env.eq_ignore_ascii_case(name) || projected.is_some_and(|p| env == p))
        })
        .map(|d| d.name.to_string())
        .collect();
    hits.dedup();
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[repo]
path = "."

[[secret]]
name = "pr_token"
env = "AUTORESEARCH_PR_TOKEN"

[[secret]]
name = "registry"
kind = "registry_authfile"
path = "/etc/quay/push.json"

[[secret]]
name = "bare"
"#;

    #[test]
    fn a_manifest_declares_names_kinds_and_projections() {
        let declared = parse_declared(MANIFEST).expect("parses");
        assert_eq!(declared.len(), 3);
        assert_eq!(declared[0].name.as_str(), "pr_token");
        assert_eq!(declared[0].kind, SecretKind::Opaque);
        assert_eq!(
            declared[0].projection,
            Some((ProjectionKind::Env, "AUTORESEARCH_PR_TOKEN".to_string()))
        );
        assert_eq!(declared[1].kind, SecretKind::RegistryAuthfile);
        assert_eq!(
            declared[1].projection,
            Some((ProjectionKind::File, "/etc/quay/push.json".to_string()))
        );
        assert_eq!(declared[2].projection, None);
    }

    #[test]
    fn a_manifest_without_the_block_declares_nothing() {
        assert!(
            parse_declared("[repo]\npath = \".\"\n")
                .expect("parses")
                .is_empty()
        );
    }

    #[test]
    fn a_malformed_declaration_is_an_error() {
        assert!(parse_declared("[[secret]]\nname = \"a b\"\n").is_err());
        assert!(parse_declared("[[secret]]\nname = \"a\"\nkind = \"nope\"\n").is_err());
        assert!(
            parse_declared("[[secret]]\nname = \"a\"\nenv = \"A\"\npath = \"/p\"\n").is_err(),
            "one projection or the other, never both"
        );
    }

    #[test]
    fn an_absent_manifest_declares_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            declared_secrets(dir.path())
                .expect("no manifest is no error")
                .is_empty()
        );
    }

    #[test]
    fn a_manifest_on_disk_is_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("crucible.toml"), MANIFEST).expect("write");
        assert_eq!(declared_secrets(dir.path()).expect("parses").len(), 3);
    }

    #[test]
    fn the_profiles_secret_env_names_are_read() {
        let profile = r#"
[secrets]
pull_authfile = "quay-authfile"

[[secret_env]]
name   = "GCLOUD_CREDENTIALS"
secret = "crucible-vertex-adc"
key    = "adc.json"

[[secret_env]]
name   = "AUTORESEARCH_PR_TOKEN"
secret = "crucible-github"
key    = "token"
"#;
        assert_eq!(
            parse_secret_env(profile),
            vec![
                "GCLOUD_CREDENTIALS".to_string(),
                "AUTORESEARCH_PR_TOKEN".to_string()
            ]
        );
        assert!(parse_secret_env("not = toml = at all").is_empty());
    }

    #[test]
    fn a_name_the_profile_also_fills_is_a_conflict() {
        let declared = parse_declared(MANIFEST).expect("parses");
        let profile = vec![
            "AUTORESEARCH_PR_TOKEN".to_string(),
            "SOMETHING_ELSE".to_string(),
        ];
        assert_eq!(conflicting_names(&declared, &profile), vec!["pr_token"]);
        assert!(conflicting_names(&declared, &["NOPE".to_string()]).is_empty());
    }

    /// The declared name itself, not only its projection, collides: a profile that fills `BARE`
    /// and a pack that declares `bare` are the same credential under two owners.
    #[test]
    fn the_declared_name_collides_case_insensitively() {
        let declared = parse_declared("[[secret]]\nname = \"bare\"\n").expect("parses");
        assert_eq!(
            conflicting_names(&declared, &["BARE".to_string()]),
            vec!["bare"]
        );
    }

    /// The declarations are frozen onto the import row as JSON, so the shape has to survive a
    /// round trip through it — a name that comes back unvalidated would reach a binding.
    #[test]
    fn declared_secrets_round_trip_through_json() {
        let declared = parse_declared(
            r#"
            [[secret]]
            name = "pr_token"
            env = "AUTORESEARCH_PR_TOKEN"

            [[secret]]
            name = "registry"
            kind = "registry_authfile"
            path = "/etc/quay/push.json"

            [[secret]]
            name = "unprojected"
            "#,
        )
        .expect("parses");
        let json = serde_json::to_value(&declared).expect("serializes");
        let back: Vec<DeclaredSecret> = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, declared);
        assert_eq!(back[2].projection, None);
    }

    #[test]
    fn a_json_declaration_with_an_unusable_name_is_refused() {
        let json =
            serde_json::json!([{"name": "not a name", "kind": "opaque", "projection": null}]);
        assert!(serde_json::from_value::<Vec<DeclaredSecret>>(json).is_err());
    }
}

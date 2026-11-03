//! The environment an agent turn sees, assembled in one place with the source of every variable.
//! Precedence is the order the methods run in: what the manifest states is never overwritten by
//! a relay, and only the turn's own inference and telemetry settings replace it.

use crucible::harness::HarnessSpec;
use std::collections::BTreeMap;

/// Where a variable came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnvSource {
    /// The manifest's `[agent].env`.
    Manifest,
    /// The Vertex selectors relayed from the engine's own environment.
    VertexRelay,
    /// The git author and committer identity the controller named for the run.
    IdentityRelay,
    /// A `[[secret]]` with an env projection the registry lets this agent hold.
    DeclaredSecret { name: String },
    /// The model endpoint and key the turn resolved.
    Inference,
    /// The in-process OTLP collector's exporter matrix.
    Otel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub key: String,
    pub value: String,
    pub source: EnvSource,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SandboxEnv {
    entries: Vec<Entry>,
}

/// The engine's own process environment, the relay source outside tests.
pub(crate) fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// The comma-separated env var names the registry lets this agent hold.
pub(crate) const AGENT_VISIBLE_ENV: &str = "CRUCIBLE_AGENT_VISIBLE_ENV";

/// The names [`AGENT_VISIBLE_ENV`] lists; unset or empty lists nothing.
pub(crate) fn agent_visible(lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
    lookup(AGENT_VISIBLE_ENV)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

impl SandboxEnv {
    pub(crate) fn from_manifest<'a>(env: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        SandboxEnv {
            entries: env
                .into_iter()
                .map(|(key, value)| Entry {
                    key: key.to_string(),
                    value: value.to_string(),
                    source: EnvSource::Manifest,
                })
                .collect(),
        }
    }

    fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Add each of `keys` that `lookup` answers with a non-empty value and that nothing has set
    /// yet. Returns the keys that crossed.
    pub(crate) fn relay<'k>(
        &mut self,
        source: EnvSource,
        keys: impl IntoIterator<Item = &'k str>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Vec<&'k str> {
        let mut crossed = Vec::new();
        for key in keys {
            if self.has(key) {
                continue;
            }
            if let Some(value) = lookup(key).filter(|v| !v.is_empty()) {
                self.entries.push(Entry {
                    key: key.to_string(),
                    value,
                    source: source.clone(),
                });
                crossed.push(key);
            }
        }
        crossed
    }

    /// Relay the env projections of `secrets` that `allowed` names, each under its own name. A
    /// secret projected to a file relays nothing here.
    pub(crate) fn relay_secrets(
        &mut self,
        secrets: &[crate::manifest::SecretDecl],
        allowed: &[String],
        lookup: impl Fn(&str) -> Option<String>,
    ) {
        for secret in secrets {
            let Some(key) = secret
                .env
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty() && allowed.iter().any(|a| a == k))
            else {
                continue;
            };
            self.relay(
                EnvSource::DeclaredSecret {
                    name: secret.name.clone(),
                },
                [key],
                &lookup,
            );
        }
    }

    /// Set `key`, replacing whatever set it before.
    pub(crate) fn set(&mut self, source: EnvSource, key: &str, value: &str) {
        self.entries.retain(|e| e.key != key);
        self.entries.push(Entry {
            key: key.to_string(),
            value: value.to_string(),
            source,
        });
    }

    pub(crate) fn remove(&mut self, keys: &[&str]) {
        self.entries.retain(|e| !keys.contains(&e.key.as_str()));
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }

    pub(crate) fn vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|e| (e.key.as_str(), e.value.as_str()))
    }

    pub(crate) fn into_vars(self) -> Vec<(String, String)> {
        self.entries.into_iter().map(|e| (e.key, e.value)).collect()
    }

    /// The declared secrets that crossed, as `[[secret]].name -> env var`.
    pub(crate) fn relayed_secrets(&self) -> BTreeMap<String, String> {
        self.entries
            .iter()
            .filter_map(|e| match &e.source {
                EnvSource::DeclaredSecret { name } => Some((name.clone(), e.key.clone())),
                _ => None,
            })
            .collect()
    }

    /// The agent's env script, sourced before the agent runs in the sandbox: `AGENT_TOOL`, the
    /// harness's home var, its further unquoted exports, then every entry here, quoted.
    pub(crate) fn script(&self, spec: &HarnessSpec) -> String {
        let mut lines = vec![
            format!("export AGENT_TOOL={}", spec.name),
            format!("export {}={}", spec.home_var, spec.home),
        ];
        lines.extend(
            spec.sandbox_env
                .iter()
                .map(|(k, v)| format!("export {k}={v}")),
        );
        lines.extend(
            self.vars()
                .map(|(key, value)| format!("export {key}={}", sh_quote(value))),
        );
        lines.push(String::new());
        lines.join("\n")
    }
}

/// Single-quote a value for `sh` (wrap in `'…'`, escaping embedded single quotes), like
/// `shlex.quote`.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use crate::agent::sandbox_env::*;
    use crate::manifest::Harness;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn relay_copies_set_nonempty_keys_and_skips_empty_and_unset() {
        let mut env = SandboxEnv::from_manifest([]);
        let crossed = env.relay(
            EnvSource::VertexRelay,
            ["A", "B", "C"],
            lookup(&[("A", "1"), ("B", "")]),
        );
        assert_eq!(crossed, ["A"]);
        assert_eq!(env.into_vars(), [("A".to_string(), "1".to_string())]);
    }

    #[test]
    fn manifest_provided_values_win_over_the_process_env() {
        let mut env = SandboxEnv::from_manifest([("A", "manifest")]);
        env.relay(EnvSource::VertexRelay, ["A"], lookup(&[("A", "process")]));
        assert_eq!(env.get("A"), Some("manifest"));
    }

    fn decl(name: &str, env: Option<&str>) -> crate::manifest::SecretDecl {
        let toml = match env {
            Some(e) => format!("name = \"{name}\"\nenv = \"{e}\"\n"),
            None => format!("name = \"{name}\"\npath = \"/tmp/{name}\"\n"),
        };
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn only_the_named_secret_projections_reach_the_agent() {
        let mut env = SandboxEnv::from_manifest([]);
        env.relay_secrets(
            &[
                decl("jira", Some("JIRA_TOKEN")),
                decl("quay", Some("QUAY_TOKEN")),
                decl("gh", Some("GH_TOKEN")),
                decl("kube", None),
            ],
            &agent_visible(lookup(&[(AGENT_VISIBLE_ENV, "JIRA_TOKEN, GH_TOKEN,,kube")])),
            lookup(&[
                ("JIRA_TOKEN", "j"),
                ("QUAY_TOKEN", "q"),
                ("GH_TOKEN", ""),
                ("kube", "k"),
            ]),
        );
        assert_eq!(env.get("JIRA_TOKEN"), Some("j"));
        assert_eq!(env.get("QUAY_TOKEN"), None);
        assert_eq!(env.get("GH_TOKEN"), None);
        assert_eq!(env.get("kube"), None, "a file projection relays nothing");
        assert_eq!(
            env.relayed_secrets(),
            BTreeMap::from([("jira".to_string(), "JIRA_TOKEN".to_string())])
        );
        assert!(agent_visible(lookup(&[])).is_empty());
    }

    #[test]
    fn a_manifest_value_wins_over_a_relayed_secret() {
        let mut env = SandboxEnv::from_manifest([("JIRA_TOKEN", "manifest")]);
        env.relay_secrets(
            &[decl("jira", Some("JIRA_TOKEN"))],
            &["JIRA_TOKEN".to_string()],
            lookup(&[("JIRA_TOKEN", "process")]),
        );
        assert_eq!(env.get("JIRA_TOKEN"), Some("manifest"));
        assert!(env.relayed_secrets().is_empty());
    }

    #[test]
    fn sh_quote_wraps_and_escapes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn set_replaces_and_remove_drops_whatever_source_wrote_them() {
        let mut env = SandboxEnv::from_manifest([("CLOUD_ML_REGION", "us"), ("K", "v")]);
        env.set(EnvSource::Inference, "K", "w");
        env.remove(&["CLOUD_ML_REGION"]);
        assert_eq!(env.into_vars(), [("K".to_string(), "w".to_string())]);
    }

    #[test]
    fn the_script_exports_the_harness_defaults_then_every_entry_quoted() {
        let mut env = SandboxEnv::from_manifest([("K", "v w")]);
        env.set(EnvSource::Otel, "OTEL_X", "a'b");
        let script = env.script(Harness::Claude.spec());
        assert!(
            script.starts_with(
                "export AGENT_TOOL=claude\nexport CLAUDE_CONFIG_DIR=/sandbox/.claude\n"
            )
        );
        assert!(script.contains("export DISABLE_AUTOUPDATER=1\n"));
        assert!(script.contains("export K='v w'\n"));
        assert!(script.ends_with("export OTEL_X='a'\\''b'\n"));
    }
}

use serde::Deserialize;

fn default_true() -> bool {
    true
}

/// The OpenShell egress policy for a sandboxed turn. The sandbox is deny-by-default; these
/// merge with the built-in defaults (see [`crate::openshell::policy`]); the allowlist is the
/// domain's, owned in TOML.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct OpenshellCfg {
    /// Extra egress endpoints in openshell's `host:port:access[:proto[:enforcement]]` form,
    /// e.g. `"aiplatform.googleapis.com:443:read-write"`.
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// Binaries allowed to open egress, merged with the agent CLI. Descendants inherit a
    /// parent's egress, so usually only the root agent needs listing.
    #[serde(default)]
    pub binaries: Vec<String>,
    /// Whether `endpoints`/`binaries` extend the built-in defaults (the public forges, PyPI,
    /// Vertex, Anthropic, and the agent CLIs) or replace them outright.
    ///
    /// Set `false` for a run that must not reach the public internet: an air-gapped or
    /// private-registry deployment, or a measurement whose result the open web would
    /// contaminate (an agent that can read `github.com` can read the upstream fix for the
    /// issue it is being scored on). The allowlist then becomes exactly what this table says,
    /// **including the binaries**, omit `/usr/local/bin/claude` and the agent gets no network
    /// at all, which is a legitimate total air-gap but rarely what you meant.
    #[serde(default = "default_true")]
    pub inherit_defaults: bool,
    /// Endpoints to subtract, applied last so deny overrides both an inherited default and an
    /// appended extra. Written in the same `host:port:access[:proto[:enforcement]]` form, matched
    /// on `host:port` alone: the deny removes every entry admitting that host:port at any access
    /// level, and takes the domain with it, so `"github.com:443:full"` also removes
    /// `"*.github.com:443:full"` and `"api.github.com:443:full"`. The port is exact, denying `:443`
    /// leaves `:8443`. This is how a domain seals the contamination hole (an agent scored on an
    /// upstream issue reading the fix off GitHub) without losing the rest of the built-in
    /// allowlist the way `inherit_defaults = false` would. See
    /// [`crate::openshell::policy::resolve_endpoints`] for the full rule.
    #[serde(default)]
    pub deny_endpoints: Vec<String>,
    /// Binaries to subtract, applied last. Exact-string match against the resolved path.
    #[serde(default)]
    pub deny_binaries: Vec<String>,
    /// Absolute image paths the sandboxed agent may READ and EXECUTE, on top of its workspace.
    ///
    /// A sandbox is landlock-confined to its workdir, so anything the domain image ships outside it
    /// — a vendored pipeline, the nu toolbox under `/opt` — is `Permission denied` to the agent even
    /// though the file is world-readable and `oc exec` reads it fine. The failure is quiet: the
    /// agent reports the tool as missing and improvises, which is how a run silently stops using
    /// the method its brief mandates. List those paths here.
    ///
    /// This is a real widening of the sandbox: everything listed is readable AND executable for the
    /// whole turn (landlock's read group is Execute|ReadFile|ReadDir). Name specific directories,
    /// never `/`. Static for the sandbox's life — the gateway rejects changing it after creation.
    #[serde(default)]
    pub read_only_paths: Vec<String>,
}

impl Default for OpenshellCfg {
    /// Inheriting the defaults is the default: a manifest with no `[agent.openshell]` table
    /// gets the built-in allowlist, not an empty one.
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            binaries: Vec::new(),
            inherit_defaults: true,
            deny_endpoints: Vec::new(),
            deny_binaries: Vec::new(),
            read_only_paths: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;

    #[test]
    fn openshell_inherit_defaults_round_trips() {
        let base = r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [agent]
            backend = "openshell"
            goal = "g"
        "#;
        // Absent table, and a present table that says nothing about it: both inherit.
        let m: Manifest = toml::from_str(base).unwrap();
        assert!(m.agent.openshell.inherit_defaults);
        let m: Manifest =
            toml::from_str(&format!("{base}\n            [agent.openshell]\n")).unwrap();
        assert!(
            m.agent.openshell.inherit_defaults,
            "an empty table inherits"
        );

        // The air-gap opt-out. `deny_unknown_fields` is on, so this also proves the key's name.
        let m: Manifest = toml::from_str(&format!(
            "{base}
            [agent.openshell]
            inherit_defaults = false
            endpoints = [\"registry.internal:443:read-only\"]
            binaries = [\"/usr/local/bin/claude\"]
        "
        ))
        .unwrap();
        assert!(!m.agent.openshell.inherit_defaults);
    }
}

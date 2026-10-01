//! The resolved exposure of a frozen pack: what it may write (RFC-0001:C-OUTPUTS) and what reach
//! it holds (RFC-0001:C-CAPABILITY-DISCLOSURE).
//!
//! Everything here is computed from the manifest plus the engine's own environment. No pack
//! content executes, so `crucible check` and `crucible plan exposure` can print a pack's reach
//! before anything of the pack has run.

use crate::manifest::{CapabilitiesCfg, CredentialContext, Manifest, McpSet, McpSource};
use crate::openshell::gateway::ComputeDriver;
use crucible_contract::outputs::{BoundSource, ResolvedOutputs, ResolvedTarget};
use serde::Serialize;
use std::collections::BTreeMap;

/// The exposure document's wire version.
pub const EXPOSURE_VERSION: u8 = 1;

/// One disclosed capability. Externally untagged so each entry carries its own `kind` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Capability {
    /// Egress the sandbox holds, in C-EGRESS's terms.
    Egress {
        host: String,
        port: u16,
        access: String,
        source: EgressSource,
    },
    /// A credential the run holds, and what it authorizes.
    Credential {
        name: String,
        context: CredentialContext,
        system: String,
        scope: String,
    },
    /// A relay file materialized into the sandbox from host-side sources.
    Relay { path: String, sources: Vec<String> },
    /// A broker binary the pack substitutes for the engine's own.
    BrokerBin { bin: String },
    /// An `[mcp]` server the run starts from the loop image's catalog, what the pack hands it, and
    /// which turns reach it. `bin` is filled in only where the catalog is readable (see
    /// [`with_catalog_bins`]). A desugared `[agent.broker]` is disclosed as before, by
    /// [`Capability::BrokerBin`] and [`Capability::Sandbox::broker`].
    McpServer {
        name: String,
        catalog: String,
        bin: Option<String>,
        /// The pack's `env`, values included.
        env: BTreeMap<String, String>,
        /// The broker-held secrets handed to the server.
        secrets: Vec<String>,
        tools: Vec<String>,
        /// Whether turns without a named sandbox reach it.
        agent: bool,
        /// The named sandboxes that reach it.
        sandboxes: Vec<String>,
    },
    /// Whether the pack runs commands outside the sandbox, which hold their executor's reach.
    ExternalCommands { present: bool },
    /// A named `[agent.sandbox]` and what it provisions: its image, the declared secrets and
    /// relay files passed in, `[agent.broker]` reach, the `[mcp]` servers it reaches, and the
    /// egress it adds after the pack's deny list.
    Sandbox {
        name: String,
        image: String,
        secrets: Vec<String>,
        relays: Vec<String>,
        broker: bool,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        mcp: Vec<String>,
        egress: Vec<String>,
    },
}

/// Whether an egress entry is standing built-in reach or reach the manifest named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EgressSource {
    Builtin,
    Manifest,
}

/// The full exposure of a frozen pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Exposure {
    pub version: u8,
    pub outputs: Vec<crucible_contract::outputs::ResolvedOutput>,
    pub capabilities: Vec<Capability>,
}

/// Values the engine itself provisions into every openshell turn: standing disclosed reach the
/// pack neither declares nor can subtract. Read from the constants the provisioning paths use.
pub fn builtin_agent_credentials() -> Vec<&'static str> {
    let mut out = crate::openshell::policy::VERTEX_RELAY_KEYS.to_vec();
    out.extend_from_slice(crate::openshell::policy::IDENTITY_RELAY_KEYS);
    out.push(crate::plan::TASK_NAME_ENV);
    out
}

/// Compute the exposure of a frozen manifest.
///
/// `pr_repo` is the publish target the caller resolved (the manifest's `[publish].pr_repo` or the
/// launcher's), used only for the `draft-pr` engine default.
pub fn compute(m: &Manifest, pr_repo: Option<&str>) -> Exposure {
    Exposure {
        version: EXPOSURE_VERSION,
        outputs: resolved_outputs(m, pr_repo).outputs,
        capabilities: capabilities(m),
    }
}

/// The run's resolved output bounds, the same value the broker is handed.
pub fn resolved_outputs(m: &Manifest, pr_repo: Option<&str>) -> ResolvedOutputs {
    let defaults = crate::manifest::outputs::default_targets(
        m.publish
            .as_ref()
            .and_then(|p| p.pr_repo.as_deref())
            .or(pr_repo),
        &m.build,
    );
    crate::manifest::outputs::resolve(&m.outputs, &defaults)
}

/// Every disclosed capability, in a stable order: egress, credentials, relays, broker
/// substitution, MCP servers, the external-command statement, then the named sandboxes.
pub fn capabilities(m: &Manifest) -> Vec<Capability> {
    let mcp = m.mcp_set().unwrap_or_default();
    let mut out = egress(m, &mcp);
    out.extend(credentials(&m.agent.env, &m.capabilities));
    out.extend(m.agent.relay.iter().map(|r| Capability::Relay {
        path: r.dest.clone(),
        sources: relay_sources(r),
    }));
    if m.agent.broker.enabled && !m.agent.broker.bin.is_empty() {
        out.push(Capability::BrokerBin {
            bin: m.agent.broker.bin.clone(),
        });
    }
    out.extend(mcp_servers(&mcp));
    if runs_external_commands(m) {
        out.push(Capability::ExternalCommands { present: true });
    }
    out.extend(sandboxes(&m.agent, &mcp));
    out
}

/// One [`Capability::McpServer`] per catalog server the run starts, in key order.
fn mcp_servers(set: &McpSet) -> impl Iterator<Item = Capability> + '_ {
    set.servers.iter().filter_map(|server| {
        let McpSource::Catalog(catalog) = &server.source else {
            return None;
        };
        Some(Capability::McpServer {
            name: server.key.to_string(),
            catalog: catalog.clone(),
            bin: None,
            env: server.env.clone(),
            secrets: server.secrets.clone(),
            tools: server.tools.clone(),
            agent: set.agent.contains(&server.key),
            sandboxes: set
                .sandboxes
                .iter()
                .filter(|(_, reach)| reach.contains(&server.key))
                .map(|(name, _)| name.clone())
                .collect(),
        })
    })
}

/// Fill each catalog server's `bin` from `lookup` (the catalog this process can read). A server
/// `lookup` does not know keeps `bin` empty.
pub fn with_catalog_bins(exposure: &mut Exposure, lookup: impl Fn(&str) -> Option<String>) {
    for cap in &mut exposure.capabilities {
        if let Capability::McpServer { catalog, bin, .. } = cap {
            *bin = lookup(catalog);
        }
    }
}

fn sandboxes<'a>(
    agent: &'a crate::manifest::AgentCfg,
    mcp: &'a McpSet,
) -> impl Iterator<Item = Capability> + 'a {
    agent.sandbox.iter().map(move |(name, profile)| {
        let added = crate::manifest::OpenshellCfg {
            endpoints: profile.endpoints.clone(),
            inherit_defaults: false,
            ..agent.openshell.clone()
        };
        Capability::Sandbox {
            name: name.clone(),
            image: profile.image.trim().to_string(),
            secrets: profile.secrets.clone(),
            relays: profile.relays.clone(),
            broker: profile.broker,
            mcp: mcp
                .scope(Some(name))
                .iter()
                .filter(|key| mcp.legacy.as_ref() != Some(*key))
                .map(ToString::to_string)
                .collect(),
            egress: crate::openshell::policy::resolve_endpoints(&added, &[], &[]),
        }
    })
}

/// The credential, relay, and MCP half of the disclosure, for a shape that is not a single-repo
/// manifest. A composite's egress is its components'; what it grants directly is what its own
/// `[agent]` and `[mcp]` carry.
pub fn composite_capabilities(
    agent: &crate::manifest::AgentCfg,
    declared: &CapabilitiesCfg,
    mcp: &McpSet,
) -> Vec<Capability> {
    let mut out = credentials(&agent.env, declared);
    out.extend(agent.relay.iter().map(|r| Capability::Relay {
        path: r.dest.clone(),
        sources: relay_sources(r),
    }));
    out.extend(mcp_servers(mcp));
    out.extend(sandboxes(agent, mcp));
    out
}

/// The exposure of a composite pack. Its output bounds resolve the same way; its disclosure names
/// only what the composite itself grants, since the egress belongs to the components.
pub fn compute_composite(m: &crate::manifest::CompositeManifest) -> Exposure {
    let defaults = crate::manifest::outputs::default_targets(None, &m.build);
    Exposure {
        version: EXPOSURE_VERSION,
        outputs: crate::manifest::outputs::resolve(&m.outputs, &defaults).outputs,
        capabilities: composite_capabilities(
            &m.agent,
            &m.capabilities,
            &m.mcp_set().unwrap_or_default(),
        ),
    }
}

/// The resolved egress allowlist, each entry classified as built-in or manifest reach. With
/// `inherit_defaults = false` every entry the manifest names is manifest reach, built-in
/// lookalikes included.
fn egress(m: &Manifest, mcp: &McpSet) -> Vec<Capability> {
    let harness_defaults = crate::openshell::policy::default_endpoints(m.agent.harness);
    let resolved = crate::openshell::policy::resolve_endpoints(
        &m.agent.openshell,
        &harness_defaults,
        &mcp_endpoints(mcp),
    );
    resolved
        .iter()
        .filter_map(|entry| {
            let (host, port, access) = parse_endpoint(entry)?;
            let source = if m.agent.openshell.inherit_defaults
                && harness_defaults.contains(&entry.as_str())
            {
                EgressSource::Builtin
            } else {
                EgressSource::Manifest
            };
            Some(Capability::Egress {
                host,
                port,
                access,
                source,
            })
        })
        .collect()
}

/// The allowlist entries of the servers turns without a named sandbox reach. The sandbox reaches
/// the loop pod through the compute driver's alias; both drivers name the same pod, so the
/// disclosure states the in-cluster one.
fn mcp_endpoints(mcp: &McpSet) -> Vec<String> {
    let host = ComputeDriver::Kubernetes.broker_host();
    mcp.servers
        .iter()
        .filter(|server| mcp.agent.contains(&server.key))
        .filter_map(|server| {
            let url = match &server.source {
                McpSource::Broker(cfg) => crate::manifest::resolve_broker_url(cfg, host),
                McpSource::Catalog(_) => format!("http://{host}:{}/mcp", server.port),
            };
            crate::manifest::broker_endpoint_from_url(&url).ok()
        })
        .collect()
}

/// Split an openshell `host:port:access[:proto[:enforcement]]` entry. A bracketed IPv6 literal
/// keeps its colons inside the brackets.
fn parse_endpoint(entry: &str) -> Option<(String, u16, String)> {
    let (host, rest) = if let Some(after) = entry.strip_prefix('[') {
        let close = after.find(']')?;
        (format!("[{}]", &after[..close]), after.get(close + 2..)?)
    } else {
        let colon = entry.find(':')?;
        (entry[..colon].to_string(), entry.get(colon + 1..)?)
    };
    let mut fields = rest.split(':');
    let port: u16 = fields.next()?.parse().ok()?;
    let access = fields.next().unwrap_or("full").to_string();
    Some((host, port, access))
}

/// Credentials: every `[agent].env` name, plus every declared secret the env does not already
/// name. An `[agent].env` name with no declaration is disclosed with its reach unstated, which
/// `crucible check` warns about.
fn credentials(env: &BTreeMap<String, String>, declared: &CapabilitiesCfg) -> Vec<Capability> {
    let mut out: Vec<Capability> = env
        .keys()
        .map(|name| match declared.secret_named(name) {
            Some(d) => Capability::Credential {
                name: d.name.clone(),
                context: d.context,
                system: d.system.clone(),
                scope: d.scope.clone(),
            },
            None => Capability::Credential {
                name: name.clone(),
                context: CredentialContext::Agent,
                system: UNDECLARED.to_string(),
                scope: UNDECLARED.to_string(),
            },
        })
        .collect();
    out.extend(
        declared
            .secret
            .iter()
            .filter(|d| !env.contains_key(&d.name))
            .map(|d| Capability::Credential {
                name: d.name.clone(),
                context: d.context,
                system: d.system.clone(),
                scope: d.scope.clone(),
            }),
    );
    out
}

/// What a credential's `system`/`scope` read as when `[agent].env` names it and no
/// `[[capabilities.secret]]` states its reach.
pub const UNDECLARED: &str = "undeclared";

/// A relay's host-side sources, as the disclosure names them.
fn relay_sources(r: &crate::manifest::RelayFile) -> Vec<String> {
    let mut out = Vec::new();
    if r.template.is_some() {
        out.push("template".to_string());
    }
    if let Some(path) = &r.from_file {
        out.push(format!("file:{path}"));
    }
    if let Some(cmd) = &r.from_cmd {
        out.push(format!("cmd:{cmd}"));
    }
    out
}

/// Whether the pack runs commands outside the sandbox: workflow `command`/`evaluate` tasks, or
/// world/judge hooks. They hold their executor's reach, not the sandbox's.
fn runs_external_commands(m: &Manifest) -> bool {
    let hooks = [
        m.world.apply_cmd.as_ref(),
        m.world.snapshot_cmd.as_ref(),
        m.world.restore_cmd.as_ref(),
        m.judge.as_ref().map(|j| &j.measure_cmd),
    ];
    hooks.into_iter().flatten().any(|c| !c.trim().is_empty())
        || m.workflow
            .as_ref()
            .is_some_and(crate::plan::workflow::WorkflowCfg::runs_host_commands)
}

/// The grants a resolved disclosure covers, by kind. A grant supplied from outside the pack (a
/// secret binding at launch, a relayed credential) is covered when a disclosed capability of the
/// same kind has reach equal to or broader than the grant's; for a named credential or a relay
/// destination, that reduces to the name appearing here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Covered {
    pub credentials: std::collections::BTreeSet<String>,
    pub relays: std::collections::BTreeSet<String>,
}

/// A grant the run would provision that the disclosure does not cover.
#[derive(Debug, thiserror::Error)]
#[error(
    "run start refused: {kind} grant `{name}` is not covered by the pack's capability \
     disclosure ({missing}); disclose it or drop the grant"
)]
pub struct UncoveredGrant {
    pub kind: &'static str,
    pub name: String,
    pub missing: String,
}

/// What the frozen manifest's disclosure covers, engine-provisioned credentials included.
pub fn covered(m: &Manifest) -> Covered {
    covered_from(capabilities(m))
}

/// The same, from an already-computed disclosure (the composite path builds its own).
pub fn covered_from(disclosed: Vec<Capability>) -> Covered {
    let mut out = Covered {
        credentials: builtin_agent_credentials()
            .into_iter()
            .map(str::to_string)
            .collect(),
        relays: Default::default(),
    };
    for cap in disclosed {
        match cap {
            Capability::Credential { name, .. } => {
                out.credentials.insert(name);
            }
            Capability::Relay { path, .. } => {
                out.relays.insert(path);
            }
            _ => {}
        }
    }
    out
}

/// Refuse, at run start, any agent-visible value or relay-materialized file the disclosure does
/// not cover.
pub fn refuse_uncovered(
    covered: &Covered,
    env: &[(String, String)],
    relays: &[crate::manifest::RelayFile],
) -> Result<(), UncoveredGrant> {
    if let Some((name, _)) = env
        .iter()
        .find(|(name, _)| !covered.credentials.contains(name))
    {
        return Err(UncoveredGrant {
            kind: "credential",
            name: name.clone(),
            missing: format!("no [[capabilities.secret]] or [agent].env entry names {name}"),
        });
    }
    if let Some(rf) = relays.iter().find(|rf| !covered.relays.contains(&rf.dest)) {
        return Err(UncoveredGrant {
            kind: "relay",
            name: rf.dest.clone(),
            missing: format!("no [[agent.relay]] entry materializes {}", rf.dest),
        });
    }
    Ok(())
}

/// Human-readable rendering for `crucible check`.
pub fn render(exposure: &Exposure) -> Vec<String> {
    let mut lines = vec!["output bounds:".to_string()];
    for out in &exposure.outputs {
        let source = match out.source {
            BoundSource::Manifest => "manifest",
            BoundSource::EngineDefault => "engine default",
        };
        let target = match &out.target {
            Some(ResolvedTarget::Fixed { fixed }) => format!("target {fixed}"),
            Some(ResolvedTarget::Open { open }) => match &open.param {
                Some(p) => format!("open target within {} bound to param {p}", open.scope),
                None => format!("open target within {}", open.scope),
            },
            None if out.kind.addresses_target() => "NO TARGET (every write refused)".to_string(),
            None => "addresses nothing".to_string(),
        };
        lines.push(format!(
            "  {:<18} count {:<4} {target} ({source})",
            out.kind.as_str(),
            out.count
        ));
    }
    lines.push("capability disclosure:".to_string());
    for cap in &exposure.capabilities {
        lines.push(format!("  {}", render_capability(cap)));
    }
    lines
}

fn render_capability(cap: &Capability) -> String {
    match cap {
        Capability::Egress {
            host,
            port,
            access,
            source,
        } => {
            let source = match source {
                EgressSource::Builtin => "builtin",
                EgressSource::Manifest => "manifest",
            };
            format!("egress      {host}:{port} {access} ({source})")
        }
        Capability::Credential {
            name,
            context,
            system,
            scope,
        } => format!("credential  {name} [{context}] on {system}: {scope}"),
        Capability::Relay { path, sources } => {
            format!("relay       {path} from {}", sources.join(", "))
        }
        Capability::BrokerBin { bin } => format!("broker-bin  {bin}"),
        Capability::McpServer {
            name,
            catalog,
            bin,
            env,
            secrets,
            tools,
            agent,
            sandboxes,
        } => {
            let source = match bin {
                Some(b) => format!("catalog {catalog} ({b})"),
                None => format!("catalog {catalog}"),
            };
            let mut reach: Vec<&str> = Vec::new();
            if *agent {
                reach.push("[agent]");
            }
            reach.extend(sandboxes.iter().map(String::as_str));
            let mut line = format!(
                "mcp         {name} from {source}; reached by [{}]",
                reach.join(", ")
            );
            if !secrets.is_empty() {
                line.push_str(&format!("; secrets [{}]", secrets.join(", ")));
            }
            if !env.is_empty() {
                let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                line.push_str(&format!("; env [{}]", env.join(", ")));
            }
            if !tools.is_empty() {
                line.push_str(&format!("; tools [{}]", tools.join(", ")));
            }
            line
        }
        Capability::ExternalCommands { present: true } => {
            "external    this pack runs commands outside the sandbox, holding their executor's reach"
                .to_string()
        }
        Capability::ExternalCommands { present: false } => {
            "external    no commands run outside the sandbox".to_string()
        }
        Capability::Sandbox {
            name,
            image,
            secrets,
            relays,
            broker,
            mcp,
            egress,
        } => format!(
            "sandbox     {name} from {image}; secrets [{}], relays [{}], broker {}, mcp [{}], \
             adds egress [{}]",
            secrets.join(", "),
            relays.join(", "),
            if *broker { "yes" } else { "no" },
            mcp.join(", "),
            egress.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::exposure::*;

    fn manifest(text: &str) -> Manifest {
        toml::from_str(text).expect("manifest parses")
    }

    const OPENSHELL: &str = r#"
        [repo]
        path = "."
        [agent]
        backend = "openshell"
        goal = "g"
        [judge]
        measure_cmd = "./m"
        direction = "higher"
    "#;

    #[test]
    fn a_named_sandbox_is_disclosed_with_what_it_provisions() {
        let m = manifest(&format!(
            "{OPENSHELL}
            [agent.openshell]
            deny_endpoints = [\"evil.example:443:full\"]
            [agent.sandbox.go]
            image = \" ghcr.io/acme/go@sha256:bb \"
            secrets = [\"registry\"]
            endpoints = [\"proxy.golang.org:443:read-only\", \"evil.example:443:full\"]
            [agent.sandbox.bare]
            image = \"ghcr.io/acme/bare@sha256:cc\"
            "
        ));
        let sandboxes: Vec<Capability> = capabilities(&m)
            .into_iter()
            .filter(|c| matches!(c, Capability::Sandbox { .. }))
            .collect();
        assert_eq!(
            sandboxes,
            [
                Capability::Sandbox {
                    name: "bare".into(),
                    image: "ghcr.io/acme/bare@sha256:cc".into(),
                    secrets: vec![],
                    relays: vec![],
                    broker: false,
                    mcp: vec![],
                    egress: vec![],
                },
                Capability::Sandbox {
                    name: "go".into(),
                    image: "ghcr.io/acme/go@sha256:bb".into(),
                    secrets: vec!["registry".into()],
                    relays: vec![],
                    broker: false,
                    mcp: vec![],
                    egress: vec!["proxy.golang.org:443:read-only".into()],
                },
            ],
            "the pack's deny list binds a sandbox's egress too"
        );
    }

    #[test]
    fn the_exposure_json_carries_exactly_the_declared_top_level_shape() {
        let m = manifest(OPENSHELL);
        let json = serde_json::to_value(compute(&m, None)).expect("serializes");
        let obj = json.as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["capabilities", "outputs", "version"]);
        assert_eq!(json["version"], 1);
        assert!(json["outputs"].is_array());
        assert!(json["capabilities"].is_array());
        for out in json["outputs"].as_array().expect("array") {
            assert!(out["kind"].is_string());
            assert!(out["count"].is_u64());
        }
    }

    #[test]
    fn builtin_egress_is_disclosed_and_manifest_extras_are_labelled() {
        let m = manifest(&format!(
            "{OPENSHELL}\n[agent.openshell]\nendpoints = [\"registry.internal:443:read-only\"]\n"
        ));
        let caps = capabilities(&m);
        let builtin = caps
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    Capability::Egress {
                        source: EgressSource::Builtin,
                        ..
                    }
                )
            })
            .count();
        assert!(builtin > 0, "the standing allowlist is disclosed reach");
        let extra = caps
            .iter()
            .find(|c| matches!(c, Capability::Egress { host, .. } if host == "registry.internal"))
            .expect("the manifest entry is disclosed");
        assert_eq!(
            extra,
            &Capability::Egress {
                host: "registry.internal".into(),
                port: 443,
                access: "read-only".into(),
                source: EgressSource::Manifest,
            }
        );
    }

    #[test]
    fn with_inherit_defaults_false_every_entry_is_manifest_reach() {
        let m = manifest(&format!(
            "{OPENSHELL}\n[agent.openshell]\ninherit_defaults = false\nendpoints = [\"github.com:443:full\"]\n"
        ));
        let caps = capabilities(&m);
        let entries: Vec<&Capability> = caps
            .iter()
            .filter(|c| matches!(c, Capability::Egress { .. }))
            .collect();
        assert_eq!(entries.len(), 1, "only what the manifest lists");
        assert!(matches!(
            entries[0],
            Capability::Egress {
                source: EgressSource::Manifest,
                ..
            }
        ));
    }

    #[test]
    fn agent_env_names_are_credentials_and_a_declaration_states_their_reach() {
        let m = manifest(&format!(
            "{OPENSHELL}
            [agent.env]
            JIRA_API_TOKEN = \"x\"
            RIG_API_URL = \"y\"
            [[capabilities.secret]]
            name = \"JIRA_API_TOKEN\"
            context = \"agent\"
            system = \"jira\"
            scope = \"comment on PROJ\"
            [[capabilities.secret]]
            name = \"QUAY_PUSH\"
            context = \"broker\"
            system = \"quay.io\"
            scope = \"push to aipcc/*\"
        "
        ));
        let caps = capabilities(&m);
        assert!(caps.contains(&Capability::Credential {
            name: "JIRA_API_TOKEN".into(),
            context: CredentialContext::Agent,
            system: "jira".into(),
            scope: "comment on PROJ".into(),
        }));
        assert!(
            caps.contains(&Capability::Credential {
                name: "RIG_API_URL".into(),
                context: CredentialContext::Agent,
                system: UNDECLARED.into(),
                scope: UNDECLARED.into(),
            }),
            "an undeclared agent value is still disclosed"
        );
        assert!(
            caps.contains(&Capability::Credential {
                name: "QUAY_PUSH".into(),
                context: CredentialContext::Broker,
                system: "quay.io".into(),
                scope: "push to aipcc/*".into(),
            }),
            "a broker-held credential need not appear in [agent].env"
        );
    }

    #[test]
    fn relays_and_a_substituted_broker_binary_are_disclosed() {
        let m = manifest(&format!(
            "{OPENSHELL}
            [[agent.relay]]
            dest = \".kube/config\"
            from_cmd = \"kubectl config view --raw\"
            [agent.broker]
            enabled = true
            bin = \"my-broker\"
        "
        ));
        let caps = capabilities(&m);
        assert!(caps.contains(&Capability::Relay {
            path: ".kube/config".into(),
            sources: vec!["cmd:kubectl config view --raw".into()],
        }));
        assert!(caps.contains(&Capability::BrokerBin {
            bin: "my-broker".into()
        }));
        assert!(
            caps.iter().any(
                |c| matches!(c, Capability::Egress { host, .. } if host.contains("openshell.internal"))
            ),
            "an enabled broker's endpoint is disclosed reach"
        );
    }

    #[test]
    fn a_judge_or_world_hook_states_that_commands_run_outside_the_sandbox() {
        let m = manifest(OPENSHELL);
        assert!(capabilities(&m).contains(&Capability::ExternalCommands { present: true }));
        let no_hooks = manifest(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "g"
        "#,
        );
        assert!(
            !capabilities(&no_hooks)
                .iter()
                .any(|c| matches!(c, Capability::ExternalCommands { .. })),
            "a pack with no host-side commands makes no such statement"
        );
    }

    #[test]
    fn a_grant_the_disclosure_does_not_cover_is_refused_naming_both() {
        let m = manifest(&format!("{OPENSHELL}\n[agent.env]\nRIG_API_URL = \"y\"\n"));
        let covered = covered(&m);
        assert!(
            refuse_uncovered(
                &covered,
                &[("RIG_API_URL".into(), "y".into())],
                &m.agent.relay
            )
            .is_ok()
        );
        let err = refuse_uncovered(&covered, &[("SNEAKY_TOKEN".into(), "x".into())], &[])
            .expect_err("uncovered credential");
        assert_eq!(err.kind, "credential");
        assert_eq!(err.name, "SNEAKY_TOKEN");
        assert!(err.to_string().contains("SNEAKY_TOKEN"), "{err}");
        assert!(err.to_string().contains("capabilities.secret"), "{err}");
    }

    #[test]
    fn the_engines_own_relayed_model_config_is_standing_disclosed_reach() {
        let m = manifest(OPENSHELL);
        let grants: Vec<(String, String)> = builtin_agent_credentials()
            .into_iter()
            .map(|k| (k.to_string(), "v".to_string()))
            .collect();
        assert!(refuse_uncovered(&covered(&m), &grants, &[]).is_ok());
    }

    #[test]
    fn the_run_identity_the_controller_names_is_not_a_credential_grant() {
        let m = manifest(OPENSHELL);
        let grants: Vec<(String, String)> = crate::openshell::policy::IDENTITY_RELAY_KEYS
            .iter()
            .map(|k| (k.to_string(), "crucible-bot[bot]".to_string()))
            .collect();
        assert!(refuse_uncovered(&covered(&m), &grants, &m.agent.relay).is_ok());
    }

    #[test]
    fn the_engines_own_task_name_value_does_not_refuse_a_playbook_turn() {
        let m = manifest(OPENSHELL);
        let grants = vec![(crate::plan::TASK_NAME_ENV.to_string(), "audit".to_string())];
        assert!(refuse_uncovered(&covered(&m), &grants, &m.agent.relay).is_ok());
    }

    #[test]
    fn a_relay_the_pack_never_declared_is_refused() {
        let m = manifest(OPENSHELL);
        let grant = crate::manifest::RelayFile {
            dest: ".aws/credentials".into(),
            from_cmd: Some("cat /host/creds".into()),
            ..Default::default()
        };
        let err = refuse_uncovered(&covered(&m), &[], std::slice::from_ref(&grant))
            .expect_err("uncovered relay");
        assert_eq!(err.kind, "relay");
        assert_eq!(err.name, ".aws/credentials");
        let declared = manifest(&format!(
            "{OPENSHELL}\n[[agent.relay]]\ndest = \".aws/credentials\"\nfrom_cmd = \"cat /host/creds\"\n"
        ));
        assert!(refuse_uncovered(&covered(&declared), &[], &[grant]).is_ok());
    }

    #[test]
    fn endpoint_parsing_handles_ports_access_and_ipv6() {
        assert_eq!(
            parse_endpoint("github.com:443:full"),
            Some(("github.com".into(), 443, "full".into()))
        );
        assert_eq!(
            parse_endpoint("api.x:8443:read-only:rest:strict"),
            Some(("api.x".into(), 8443, "read-only".into()))
        );
        assert_eq!(
            parse_endpoint("[::1]:8849:full"),
            Some(("[::1]".into(), 8849, "full".into()))
        );
        assert_eq!(parse_endpoint("garbage"), None);
    }

    #[test]
    fn the_rendering_names_every_kind_and_every_capability() {
        let m = manifest(OPENSHELL);
        let text = render(&compute(&m, None)).join("\n");
        for kind in crucible_contract::outputs::OutputKind::ALL {
            assert!(text.contains(kind.as_str()), "{kind} missing from {text}");
        }
        assert!(text.contains("capability disclosure:"));
        assert!(text.contains("egress"));
    }

    const MCP: &str = r#"
        [repo]
        path = "."
        [agent]
        backend = "openshell"
        goal = "g"
        mcp = ["trace"]
        [agent.openshell]
        inherit_defaults = false
        [agent.sandbox.go]
        image = "img"
        mcp = ["jira", "trace"]
        [agent.sandbox.bare]
        image = "img"
        [[capabilities.secret]]
        name = "JIRA_TOKEN"
        context = "broker"
        system = "jira"
        scope = "comment on PROJ"
        [mcp.jira]
        catalog = "ujira"
        secrets = ["JIRA_TOKEN"]
        env = { JIRA_LABEL = "crucible" }
        tools = ["comment"]
        [mcp.trace]
        [mcp.idle]
    "#;

    #[test]
    fn every_started_mcp_server_is_disclosed_with_who_reaches_it() {
        let m = manifest(MCP);
        let caps = capabilities(&m);
        let servers: Vec<&Capability> = caps
            .iter()
            .filter(|c| matches!(c, Capability::McpServer { .. }))
            .collect();
        assert_eq!(
            servers,
            [
                &Capability::McpServer {
                    name: "jira".into(),
                    catalog: "ujira".into(),
                    bin: None,
                    env: BTreeMap::from([("JIRA_LABEL".into(), "crucible".into())]),
                    secrets: vec!["JIRA_TOKEN".into()],
                    tools: vec!["comment".into()],
                    agent: false,
                    sandboxes: vec!["go".into()],
                },
                &Capability::McpServer {
                    name: "trace".into(),
                    catalog: "trace".into(),
                    bin: None,
                    env: BTreeMap::new(),
                    secrets: vec![],
                    tools: vec![],
                    agent: true,
                    sandboxes: vec!["go".into()],
                },
            ],
            "an unscoped server is never started, so never disclosed"
        );
        let sandbox_mcp: Vec<(&str, &[String])> = caps
            .iter()
            .filter_map(|c| match c {
                Capability::Sandbox { name, mcp, .. } => Some((name.as_str(), mcp.as_slice())),
                _ => None,
            })
            .collect();
        assert_eq!(
            sandbox_mcp,
            [
                ("bare", &[][..]),
                ("go", &["jira".to_string(), "trace".to_string()][..])
            ]
        );
        let egress: Vec<String> = caps
            .iter()
            .filter_map(|c| match c {
                Capability::Egress { host, port, .. } => Some(format!("{host}:{port}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            egress,
            ["host.openshell.internal:8850"],
            "a profileless turn reaches trace (the second port) and nothing else"
        );
    }

    #[test]
    fn a_catalog_bin_is_filled_where_the_catalog_is_readable_and_rendered() {
        let m = manifest(MCP);
        let mut exposure = compute(&m, None);
        with_catalog_bins(&mut exposure, |name| {
            (name == "ujira").then(|| "/usr/local/bin/ujira-mcp".to_string())
        });
        let text = render(&exposure).join("\n");
        assert!(
            text.contains(
                "mcp         jira from catalog ujira (/usr/local/bin/ujira-mcp); reached by [go]; \
                 secrets [JIRA_TOKEN]; env [JIRA_LABEL=crucible]; tools [comment]"
            ),
            "{text}"
        );
        assert!(
            text.contains("mcp         trace from catalog trace; reached by [[agent], go]"),
            "{text}"
        );
        assert!(text.contains("mcp [jira, trace]"), "{text}");
    }

    #[test]
    fn a_desugared_broker_is_disclosed_exactly_as_before() {
        let m = manifest(&format!(
            "{OPENSHELL}
            [agent.broker]
            enabled = true
            bin = \"vllm-broker\"
            name = \"vllm-broker\"
            [agent.sandbox.go]
            image = \"img\"
            broker = true
            mcp = [\"trace\"]
            [agent.sandbox.bare]
            image = \"img\"
            [mcp.trace]
            "
        ));
        let caps = capabilities(&m);
        assert!(caps.contains(&Capability::BrokerBin {
            bin: "vllm-broker".into()
        }));
        let servers: Vec<&str> = caps
            .iter()
            .filter_map(|c| match c {
                Capability::McpServer { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(servers, ["trace"], "the broker is not an mcp-server");
        assert!(
            caps.iter().any(|c| matches!(
                c,
                Capability::Egress { host, port: 8849, .. } if host == "host.openshell.internal"
            )),
            "profileless turns still reach the broker: {caps:?}"
        );
        let sandboxes: Vec<serde_json::Value> = caps
            .iter()
            .filter(|c| matches!(c, Capability::Sandbox { .. }))
            .map(|c| serde_json::to_value(c).expect("serializes"))
            .collect();
        assert_eq!(
            sandboxes,
            [
                serde_json::json!({
                    "kind": "sandbox", "name": "bare", "image": "img", "secrets": [],
                    "relays": [], "broker": false, "egress": []
                }),
                serde_json::json!({
                    "kind": "sandbox", "name": "go", "image": "img", "secrets": [],
                    "relays": [], "broker": true, "mcp": ["trace"], "egress": []
                }),
            ],
            "a sandbox without [mcp] reach serializes as it did before [mcp] existed"
        );
    }

    #[test]
    fn a_pack_env_value_or_secret_changes_the_disclosure() {
        let base = compute(&manifest(MCP), None);
        let other_env = compute(&manifest(&MCP.replace("\"crucible\"", "\"other\"")), None);
        let no_secret = compute(
            &manifest(&MCP.replace("secrets = [\"JIRA_TOKEN\"]\n", "")),
            None,
        );
        assert_ne!(base, other_env);
        assert_ne!(base, no_secret);
        let text = render(&base).join("\n");
        assert!(
            text.contains("secrets [JIRA_TOKEN]; env [JIRA_LABEL=crucible]; tools [comment]"),
            "{text}"
        );
    }
}

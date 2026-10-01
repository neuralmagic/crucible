//! `[mcp.<key>]`: the MCP servers a pack declares, which turns reach each one, and the port each
//! listens on. Nothing is reached by default: `[agent].mcp` scopes turns that run without a named
//! sandbox, and `[agent.sandbox.<name>].mcp` scopes that sandbox. Only a server some scope names is
//! started.
//!
//! `[agent.broker]` still works and desugars to `[mcp.<broker.name>]` with every tool, reached by
//! turns without a named sandbox and by each sandbox with `broker = true`.

use crate::manifest::broker::{BrokerCfg, broker_port};
use crate::manifest::capability::{CapabilitiesCfg, CredentialContext};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// The port the first server listens on; the rest follow in key order.
pub const FIRST_PORT: u16 = 8849;

/// The tool selection `[agent.broker]` desugars to.
pub const ALL_TOOLS: &str = "*";

const KEY_MAX: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpError {
    #[error("[mcp.{key}] name must be 1..={KEY_MAX} characters of [a-z0-9_-]")]
    BadKey { key: String },
    #[error("[mcp.{key}].catalog {catalog:?} must be 1..={KEY_MAX} characters of [a-z0-9_-]")]
    BadCatalog { key: String, catalog: String },
    #[error(
        "[agent.broker].name {name:?} must be 1..={KEY_MAX} characters of [a-z0-9_-] to desugar to \
         [mcp.{name}]"
    )]
    BadLegacyName { name: String },
    #[error(
        "[agent.broker] desugars to [mcp.{name}], which [mcp] also declares; declare the broker \
         once, as [mcp.{name}]"
    )]
    LegacyClash { name: String },
    #[error("{scope} names MCP server {key:?}, which no [mcp.{key}] declares")]
    UnknownScope { scope: String, key: String },
    #[error(
        "[mcp.{key}] names secret {secret:?}, which no [[capabilities.secret]] declares with \
         context = \"broker\""
    )]
    SecretNotBroker { key: String, secret: String },
    #[error("[mcp.{key}].env {name:?} is not an environment variable name, or is reserved (MCP_*)")]
    BadEnv { key: String, name: String },
    #[error("[mcp.{key}].tools has an empty entry")]
    EmptyTool { key: String },
    #[error("[mcp.{key}] has no free port left after {first_port}")]
    NoPort { key: String, first_port: u16 },
    #[error(
        "[mcp.{a}] and [mcp.{b}] differ only in `-` versus `_`, so their sandbox credentials \
         would collide; rename one"
    )]
    TokenEnvClash { a: String, b: String },
}

/// A validated server key: the agent-visible MCP server name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpKey(String);

impl McpKey {
    pub fn parse(raw: &str) -> Option<Self> {
        name_ok(raw).then(|| Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The env key the sandbox's credential for this server resolves through. Unique per key
    /// within a set (see [`McpError::TokenEnvClash`]).
    pub fn token_env(&self) -> String {
        format!(
            "CRUCIBLE_MCP_TOKEN_{}",
            self.0.to_ascii_uppercase().replace('-', "_")
        )
    }
}

impl std::fmt::Display for McpKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= KEY_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn env_name_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !name.starts_with("MCP_")
}

/// One `[mcp.<key>]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCfg {
    /// The catalog entry to start (`/etc/crucible/mcp.d/<catalog>.toml`). Defaults to the key.
    #[serde(default)]
    pub catalog: Option<String>,
    /// `[[capabilities.secret]]` names with `context = "broker"`, handed to the server only.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Extra env for the server.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The tools the pack selects, handed to the server as `MCP_TOOLS`.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// What starts a server.
#[derive(Debug, Clone)]
pub enum McpSource {
    /// A loop-image catalog entry.
    Catalog(String),
    /// A desugared `[agent.broker]`: its own binary, bind, and URL override.
    Broker(BrokerCfg),
}

/// One activated server.
#[derive(Debug, Clone)]
pub struct McpServerDecl {
    pub key: McpKey,
    pub source: McpSource,
    pub secrets: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub tools: Vec<String>,
    pub port: u16,
}

/// Which servers each kind of turn may reach, as the manifest names them.
pub struct McpScopes<'a> {
    /// `[agent].mcp`: turns without a named sandbox.
    pub agent: &'a [String],
    pub sandboxes: Vec<SandboxScope<'a>>,
}

/// One `[agent.sandbox.<name>]`'s reach.
pub struct SandboxScope<'a> {
    pub name: &'a str,
    pub mcp: &'a [String],
    pub broker: bool,
}

/// The servers a run starts and who reaches them.
#[derive(Debug, Clone, Default)]
pub struct McpSet {
    /// Every server some scope names, in key order.
    pub servers: Vec<McpServerDecl>,
    /// What turns without a named sandbox reach.
    pub agent: BTreeSet<McpKey>,
    /// What each named sandbox reaches.
    pub sandboxes: BTreeMap<String, BTreeSet<McpKey>>,
    /// The key `[agent.broker]` desugared to, when the pack still uses it.
    pub legacy: Option<McpKey>,
}

impl McpSet {
    pub fn resolve(
        table: &BTreeMap<String, McpCfg>,
        broker: &BrokerCfg,
        scopes: &McpScopes<'_>,
        capabilities: &CapabilitiesCfg,
    ) -> Result<Self, McpError> {
        Self::resolve_from(table, broker, scopes, capabilities, FIRST_PORT)
    }

    /// [`McpSet::resolve`] with the first port given.
    pub fn resolve_from(
        table: &BTreeMap<String, McpCfg>,
        broker: &BrokerCfg,
        scopes: &McpScopes<'_>,
        capabilities: &CapabilitiesCfg,
        first_port: u16,
    ) -> Result<Self, McpError> {
        let mut declared: BTreeMap<McpKey, (McpSource, McpCfg)> = BTreeMap::new();
        for (raw, cfg) in table {
            let key = McpKey::parse(raw).ok_or_else(|| McpError::BadKey { key: raw.clone() })?;
            validate_cfg(&key, cfg, capabilities)?;
            let catalog = cfg.catalog.clone().unwrap_or_else(|| raw.clone());
            declared.insert(key, (McpSource::Catalog(catalog), cfg.clone()));
        }
        let legacy = if broker.enabled {
            let key = McpKey::parse(&broker.name).ok_or_else(|| McpError::BadLegacyName {
                name: broker.name.clone(),
            })?;
            if declared.contains_key(&key) {
                return Err(McpError::LegacyClash {
                    name: broker.name.clone(),
                });
            }
            let cfg = McpCfg {
                catalog: None,
                secrets: Vec::new(),
                env: BTreeMap::new(),
                tools: vec![ALL_TOOLS.to_string()],
            };
            declared.insert(key.clone(), (McpSource::Broker(broker.clone()), cfg));
            Some(key)
        } else {
            None
        };
        let mut seen_envs: BTreeMap<String, &McpKey> = BTreeMap::new();
        for key in declared.keys() {
            if let Some(other) = seen_envs.insert(key.token_env(), key) {
                return Err(McpError::TokenEnvClash {
                    a: other.to_string(),
                    b: key.to_string(),
                });
            }
        }

        let lookup = |scope: &str, names: &[String]| -> Result<BTreeSet<McpKey>, McpError> {
            names
                .iter()
                .map(|name| {
                    McpKey::parse(name)
                        .filter(|key| declared.contains_key(key))
                        .ok_or_else(|| McpError::UnknownScope {
                            scope: scope.to_string(),
                            key: name.clone(),
                        })
                })
                .collect()
        };
        let mut agent = lookup("[agent].mcp", scopes.agent)?;
        agent.extend(legacy.clone());
        let mut sandboxes = BTreeMap::new();
        for sandbox in &scopes.sandboxes {
            let mut reach = lookup(
                &format!("[agent.sandbox.{}].mcp", sandbox.name),
                sandbox.mcp,
            )?;
            if sandbox.broker {
                reach.extend(legacy.clone());
            }
            sandboxes.insert(sandbox.name.to_string(), reach);
        }

        let active: BTreeSet<&McpKey> = agent.iter().chain(sandboxes.values().flatten()).collect();
        let mut taken: BTreeSet<u16> = BTreeSet::new();
        let mut legacy_port = None;
        if let Some((McpSource::Broker(b), _)) = legacy.as_ref().and_then(|k| declared.get(k)) {
            let port = broker_port(&b.bind).parse().unwrap_or(FIRST_PORT);
            taken.insert(port);
            legacy_port = Some(port);
        }
        let mut next = first_port;
        let mut servers = Vec::new();
        for (key, (source, cfg)) in declared {
            if !active.contains(&key) {
                continue;
            }
            let port = match (&source, legacy_port) {
                (McpSource::Broker(_), Some(port)) => port,
                _ => {
                    while taken.contains(&next) {
                        next = next.checked_add(1).ok_or_else(|| McpError::NoPort {
                            key: key.to_string(),
                            first_port,
                        })?;
                    }
                    taken.insert(next);
                    next
                }
            };
            servers.push(McpServerDecl {
                key,
                source,
                secrets: cfg.secrets,
                env: cfg.env,
                tools: cfg.tools,
                port,
            });
        }
        Ok(Self {
            servers,
            agent,
            sandboxes,
            legacy,
        })
    }

    pub fn server(&self, key: &McpKey) -> Option<&McpServerDecl> {
        self.servers.iter().find(|s| &s.key == key)
    }

    /// The ports every started server listens on, in key order.
    pub fn ports(&self) -> Vec<u16> {
        self.servers.iter().map(|s| s.port).collect()
    }

    /// What a turn reaches: the named sandbox's scope, or `[agent].mcp`'s without one.
    pub fn scope(&self, sandbox: Option<&str>) -> BTreeSet<McpKey> {
        match sandbox {
            Some(name) => self.sandboxes.get(name).cloned().unwrap_or_default(),
            None => self.agent.clone(),
        }
    }
}

fn validate_cfg(
    key: &McpKey,
    cfg: &McpCfg,
    capabilities: &CapabilitiesCfg,
) -> Result<(), McpError> {
    if let Some(catalog) = &cfg.catalog
        && !name_ok(catalog)
    {
        return Err(McpError::BadCatalog {
            key: key.to_string(),
            catalog: catalog.clone(),
        });
    }
    for secret in &cfg.secrets {
        let broker_held = capabilities
            .secret_named(secret)
            .is_some_and(|d| d.context == CredentialContext::Broker);
        if !broker_held {
            return Err(McpError::SecretNotBroker {
                key: key.to_string(),
                secret: secret.clone(),
            });
        }
    }
    if let Some(name) = cfg.env.keys().find(|name| !env_name_ok(name)) {
        return Err(McpError::BadEnv {
            key: key.to_string(),
            name: name.clone(),
        });
    }
    if cfg.tools.iter().any(|t| t.trim().is_empty()) {
        return Err(McpError::EmptyTool {
            key: key.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;
    use crate::manifest::mcp::{ALL_TOOLS, FIRST_PORT, McpError, McpKey, McpSource};

    const BASE: &str = r#"
        [repo]
        path = "."
        [agent]
        backend = "openshell"
        goal = "g"
    "#;

    fn parse(extra_agent: &str, rest: &str) -> anyhow::Result<Manifest> {
        let text = format!("{BASE}{extra_agent}\n{rest}");
        let m: Manifest = toml::from_str(&text)?;
        m.validate()?;
        Ok(m)
    }

    fn refused(extra_agent: &str, rest: &str) -> anyhow::Error {
        match parse(extra_agent, rest) {
            Ok(_) => panic!("accepted: {extra_agent} {rest}"),
            Err(e) => e,
        }
    }

    fn refusal(extra_agent: &str, rest: &str) -> McpError {
        match refused(extra_agent, rest).downcast::<McpError>() {
            Ok(e) => e,
            Err(e) => panic!("not an McpError: {e:#}"),
        }
    }

    fn keys(set: &std::collections::BTreeSet<McpKey>) -> Vec<&str> {
        set.iter().map(McpKey::as_str).collect()
    }

    const JIRA: &str = r#"
        [[capabilities.secret]]
        name = "JIRA_TOKEN"
        context = "broker"
        system = "jira"
        scope = "comment on PROJ"
    "#;

    #[test]
    fn nothing_is_reached_or_started_by_default() {
        let m = parse("", "[mcp.jira]\n[mcp.trace]\n").expect("valid");
        let set = m.mcp_set().expect("resolves");
        assert!(set.servers.is_empty(), "declared but unscoped: not started");
        assert!(set.agent.is_empty());
        assert!(set.legacy.is_none());
        let m = parse("", "").expect("valid");
        assert!(m.mcp.is_empty() && m.agent.mcp.is_empty());
        assert!(m.mcp_set().expect("resolves").servers.is_empty());
    }

    #[test]
    fn scopes_start_only_what_they_name_on_ordered_ports() {
        let m = parse(
            "mcp = [\"trace\"]",
            r#"
            [agent.sandbox.go]
            image = "img"
            mcp = ["jira", "trace"]
            [agent.sandbox.bare]
            image = "img"
            [mcp.jira]
            catalog = "ujira"
            tools = ["comment"]
            [mcp.trace]
            [mcp.unused]
            "#,
        )
        .expect("valid");
        let set = m.mcp_set().expect("resolves");
        let started: Vec<(&str, u16)> = set
            .servers
            .iter()
            .map(|s| (s.key.as_str(), s.port))
            .collect();
        assert_eq!(
            started,
            [("jira", FIRST_PORT), ("trace", FIRST_PORT + 1)],
            "key order picks the port"
        );
        assert_eq!(keys(&set.scope(None)), ["trace"]);
        assert_eq!(keys(&set.scope(Some("go"))), ["jira", "trace"]);
        assert!(set.scope(Some("bare")).is_empty());
        let jira = &set.servers[0];
        assert!(matches!(&jira.source, McpSource::Catalog(c) if c == "ujira"));
        assert_eq!(jira.tools, ["comment"]);
        assert!(
            matches!(&set.servers[1].source, McpSource::Catalog(c) if c == "trace"),
            "the catalog defaults to the key"
        );
        assert_eq!(set.ports(), [FIRST_PORT, FIRST_PORT + 1]);
    }

    #[test]
    fn a_scope_naming_an_undeclared_server_is_refused() {
        assert_eq!(
            refusal("mcp = [\"jira\"]", ""),
            McpError::UnknownScope {
                scope: "[agent].mcp".into(),
                key: "jira".into()
            }
        );
        assert_eq!(
            refusal(
                "",
                "[agent.sandbox.go]\nimage = \"img\"\nmcp = [\"nope\"]\n[mcp.jira]\n"
            ),
            McpError::UnknownScope {
                scope: "[agent.sandbox.go].mcp".into(),
                key: "nope".into()
            }
        );
    }

    #[test]
    fn a_secret_must_be_declared_broker_held() {
        let ok = parse(
            "mcp = [\"jira\"]",
            &format!("{JIRA}\n[mcp.jira]\nsecrets = [\"JIRA_TOKEN\"]\n"),
        )
        .expect("a broker-held secret is accepted");
        assert_eq!(ok.mcp_set().unwrap().servers[0].secrets, ["JIRA_TOKEN"]);

        assert_eq!(
            refusal("", "[mcp.jira]\nsecrets = [\"JIRA_TOKEN\"]\n"),
            McpError::SecretNotBroker {
                key: "jira".into(),
                secret: "JIRA_TOKEN".into()
            },
            "undeclared"
        );
        let agent_held = JIRA.replace("\"broker\"", "\"agent\"");
        assert_eq!(
            refusal(
                "",
                &format!("{agent_held}\n[mcp.jira]\nsecrets = [\"JIRA_TOKEN\"]\n")
            ),
            McpError::SecretNotBroker {
                key: "jira".into(),
                secret: "JIRA_TOKEN".into()
            },
            "declared, but the agent holds it"
        );
    }

    #[test]
    fn bad_names_are_refused() {
        for bad in ["Jira", "ji ra", "ji.ra", &"x".repeat(65)] {
            assert_eq!(
                refusal("", &format!("[mcp.\"{bad}\"]\n")),
                McpError::BadKey { key: bad.into() },
                "{bad:?}"
            );
        }
        assert!(matches!(
            refusal("", "[mcp.jira]\ncatalog = \"../etc/passwd\"\n"),
            McpError::BadCatalog { .. }
        ));
        for env in ["lower", "MCP_BIND", "1X", ""] {
            assert!(
                matches!(
                    refusal("", &format!("[mcp.jira]\nenv = {{ \"{env}\" = \"v\" }}\n")),
                    McpError::BadEnv { .. }
                ),
                "{env:?}"
            );
        }
        assert!(matches!(
            refusal("", "[mcp.jira]\ntools = [\" \"]\n"),
            McpError::EmptyTool { .. }
        ));
        assert!(matches!(
            refusal("", "[mcp.a-b]\n[mcp.a_b]\n"),
            McpError::TokenEnvClash { .. }
        ));
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let err = refused("", "[mcp.jira]\nbin = \"/bin/sh\"\n");
        assert!(err.to_string().contains("unknown field"), "{err:#}");
    }

    #[test]
    fn agent_broker_desugars_to_a_server_every_profileless_turn_reaches() {
        let m = parse(
            "[agent.broker]\nenabled = true\nbin = \"vllm-broker\"\nname = \"vllm-broker\"\nbind = \"0.0.0.0:9999\"",
            r#"
            [agent.sandbox.with]
            image = "img"
            broker = true
            [agent.sandbox.without]
            image = "img"
            [mcp.trace]
            "#,
        )
        .expect("valid");
        let set = m.mcp_set().expect("resolves");
        assert_eq!(set.legacy.as_ref().map(McpKey::as_str), Some("vllm-broker"));
        assert_eq!(keys(&set.scope(None)), ["vllm-broker"]);
        assert_eq!(keys(&set.scope(Some("with"))), ["vllm-broker"]);
        assert!(set.scope(Some("without")).is_empty());
        let broker = &set.servers[0];
        assert!(matches!(&broker.source, McpSource::Broker(b) if b.bin == "vllm-broker"));
        assert_eq!(broker.tools, [ALL_TOOLS]);
        assert_eq!(broker.port, 9999, "the broker keeps its bind port");
        assert_eq!(set.servers.len(), 1, "[mcp.trace] is unscoped");
    }

    #[test]
    fn a_desugared_broker_keeps_its_port_and_new_servers_step_around_it() {
        let m = parse(
            "mcp = [\"alpha\", \"zeta\"]\n[agent.broker]\nenabled = true\nbin = \"b\"",
            "[mcp.alpha]\n[mcp.zeta]\n",
        )
        .expect("valid");
        let set = m.mcp_set().expect("resolves");
        let ports: Vec<(&str, u16)> = set
            .servers
            .iter()
            .map(|s| (s.key.as_str(), s.port))
            .collect();
        assert_eq!(
            ports,
            [
                ("alpha", FIRST_PORT + 1),
                ("broker", FIRST_PORT),
                ("zeta", FIRST_PORT + 2)
            ]
        );
    }

    #[test]
    fn a_desugared_broker_cannot_shadow_a_declared_server() {
        assert_eq!(
            refusal(
                "[agent.broker]\nenabled = true\nbin = \"b\"\nname = \"jira\"",
                "[mcp.jira]\n"
            ),
            McpError::LegacyClash {
                name: "jira".into()
            }
        );
        assert_eq!(
            refusal(
                "[agent.broker]\nenabled = true\nbin = \"b\"\nname = \"Broker\"",
                ""
            ),
            McpError::BadLegacyName {
                name: "Broker".into()
            }
        );
    }

    #[test]
    fn port_assignment_stops_at_the_last_port() {
        let m = parse(
            "mcp = [\"alpha\", \"beta\"]\n[agent.broker]\nenabled = true\nbin = \"b\"\nbind = \"0.0.0.0:65535\"",
            "[mcp.alpha]\n[mcp.beta]\n",
        )
        .expect("valid");
        let scopes = crate::manifest::McpScopes {
            agent: &m.agent.mcp,
            sandboxes: Vec::new(),
        };
        let resolve = |first| {
            crate::manifest::McpSet::resolve_from(
                &m.mcp,
                &m.agent.broker,
                &scopes,
                &m.capabilities,
                first,
            )
        };
        let set = resolve(65533).expect("65533 and 65534 are free");
        assert_eq!(set.ports(), [65533, 65534, 65535], "alpha, beta, broker");
        assert_eq!(
            resolve(65534).expect_err("beta has nowhere to go"),
            McpError::NoPort {
                key: "beta".into(),
                first_port: 65534
            }
        );
    }

    #[test]
    fn a_disabled_broker_desugars_to_nothing() {
        let m = parse(
            "[agent.broker]\nbin = \"b\"",
            "[agent.sandbox.go]\nimage = \"img\"\nbroker = true\n",
        )
        .expect("valid");
        let set = m.mcp_set().expect("resolves");
        assert!(set.servers.is_empty() && set.legacy.is_none());
        assert!(set.scope(Some("go")).is_empty());
    }

    #[test]
    fn token_envs_are_distinct_per_key() {
        let a = McpKey::parse("ujira").unwrap();
        let b = McpKey::parse("vllm-broker").unwrap();
        assert_eq!(a.token_env(), "CRUCIBLE_MCP_TOKEN_UJIRA");
        assert_eq!(b.token_env(), "CRUCIBLE_MCP_TOKEN_VLLM_BROKER");
    }
}

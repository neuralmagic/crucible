use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The loop-pod provisioning broker (the domain's `bin`). When `enabled`, crucible
/// spawns it as a run-lifetime child for the `openshell` backend and seeds the `.mcp.json` so the
/// agent reaches it over streamable-http. Off by default, `local` / `command` backends and the
/// tests don't need it. The broker binds `0.0.0.0` (reachable from the sandbox); the hostname the
/// sandbox uses to reach it is **driver-resolved** from `ComputeDriver::broker_host()`, so the
/// domain manifest never names the transport. When `enabled`, the engine also auto-appends the
/// broker's `host:port` to the resolved egress allowlist, so a domain does not need to list it in
/// `[agent.openshell].endpoints`.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct BrokerCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Where the broker listens. Must be `0.0.0.0:<port>` so the sandbox reaches it on the bridge,
    /// not just loopback.
    #[serde(default = "default_broker_bind")]
    pub bind: String,
    /// The URL seeded into the sandbox's `.mcp.json`. `None` (the default, and the expected path)
    /// means the engine resolves it from the active compute driver's `broker_host()` plus the port
    /// from `bind`. `Some` is an explicit override the engine honors verbatim (e.g. an operator
    /// running an out-of-band broker at a known address). The egress allowlist entry is derived
    /// from the resolved URL (override or driver-computed), so they cannot disagree.
    #[serde(default)]
    pub url: Option<String>,
    /// The MCP server name seeded into the sandbox's `.mcp.json`. It becomes the agent-visible tool
    /// prefix (`mcp__<name>__<tool>`, …), so a domain whose skills/prompts hard-code a prefix must
    /// match it here. Default `"broker"`; a pack may pin its own name for existing prompts.
    #[serde(default = "default_broker_name")]
    pub name: String,
    /// The broker binary to spawn (PATH-resolved). REQUIRED when `enabled`, each domain ships its
    /// own broker binary (it injects the domain's trace-cache resolver into the generic
    /// `crucible-broker` engine), so there's no universal default.
    #[serde(default)]
    pub bin: String,
    /// Expose the engine-side build/deploy tools to the agent. Off by
    /// default (a config-tuning domain doesn't rebuild code); when on, crucible sets
    /// `BROKER_BUILD=1` on the broker so the tools build+deploy instead of returning `disabled`.
    /// The build *target* (`FORGE_*`: registry, push authfile, deploy name) is the loop pod's env.
    #[serde(default)]
    pub build: bool,
}

impl Default for BrokerCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_broker_bind(),
            url: None,
            name: default_broker_name(),
            bin: String::new(),
            build: false,
        }
    }
}

fn default_broker_bind() -> String {
    "0.0.0.0:8849".to_string()
}
fn default_broker_name() -> String {
    "broker".to_string()
}

/// The broker port, extracted from the `bind` address (the part after the last `:`).
/// Falls back to the raw `bind` string if no colon is found (degenerate, but don't panic).
pub fn broker_port(bind: &str) -> &str {
    bind.rsplit_once(':').map_or(bind, |(_, port)| port)
}

/// Build the broker URL the sandbox will use. When the manifest provides an explicit `url`,
/// honor it verbatim. Otherwise, resolve it from the active compute driver's hostname plus the
/// port from the broker's `bind` address.
pub fn resolve_broker_url(cfg: &BrokerCfg, host: &str) -> String {
    match &cfg.url {
        Some(explicit) => explicit.clone(),
        None => format!("http://{}:{}/mcp", host, broker_port(&cfg.bind)),
    }
}

/// The broker endpoint in openshell's `host:port:access` form, for egress allowlisting.
/// Derived from the resolved broker URL's authority so the allowlist and the URL the sandbox
/// actually contacts can never disagree (the bug this replaces: computing them independently
/// let an explicit `[agent.broker].url` override reach a host the allowlist didn't name).
pub fn broker_endpoint_from_url(url: &str) -> Result<String> {
    // Strip scheme to get `host[:port]/path…`.
    let authority = url
        .strip_prefix("https://")
        .map(|rest| (rest, 443u16))
        .or_else(|| url.strip_prefix("http://").map(|rest| (rest, 80u16)))
        .with_context(|| format!("broker URL must start with http:// or https://, got {url:?}"))?;
    let (rest, default_port) = authority;
    // The authority is everything before the first `/` (or the whole thing if no path).
    let host_port = rest.split('/').next().unwrap_or(rest);
    if host_port.is_empty() {
        bail!("broker URL has an empty authority: {url:?}");
    }
    // Split host and optional port. A bracketed IPv6 literal carries colons inside the brackets, so
    // it is the only case where a colon is not a port separator: `[::1]:8849` or bare `[::1]`.
    let (host, port) = if host_port.starts_with('[') {
        match host_port.find("]:") {
            Some(bracket_end) => {
                let p: u16 = host_port[bracket_end + 2..]
                    .parse()
                    .with_context(|| format!("invalid port in broker URL {url:?}"))?;
                (&host_port[..bracket_end + 1], p)
            }
            None => (host_port, default_port),
        }
    } else if let Some((h, p_str)) = host_port.rsplit_once(':') {
        // A colon on an unbracketed authority is always a port separator: a typo'd port is an error,
        // never a hostname that happens to contain a colon (RFC 3986 requires brackets for IPv6).
        let p: u16 = p_str
            .parse()
            .with_context(|| format!("invalid port in broker URL {url:?}"))?;
        (h, p)
    } else {
        (host_port, default_port)
    };
    Ok(format!("{host}:{port}:full"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    #[test]
    fn broker_url_defaults_to_none() {
        let cfg = BrokerCfg::default();
        assert!(cfg.url.is_none());
    }

    #[test]
    fn broker_url_resolves_from_podman_driver() {
        let cfg = BrokerCfg::default();
        let url = resolve_broker_url(&cfg, "host.containers.internal");
        assert_eq!(url, "http://host.containers.internal:8849/mcp");
    }

    #[test]
    fn broker_url_resolves_from_kubernetes_driver() {
        let cfg = BrokerCfg::default();
        let url = resolve_broker_url(&cfg, "host.openshell.internal");
        assert_eq!(url, "http://host.openshell.internal:8849/mcp");
    }

    #[test]
    fn broker_url_explicit_override_honored() {
        let cfg = BrokerCfg {
            url: Some("http://custom:9999/mcp".to_string()),
            ..BrokerCfg::default()
        };
        let url = resolve_broker_url(&cfg, "host.containers.internal");
        assert_eq!(url, "http://custom:9999/mcp", "explicit url wins");
    }

    #[test]
    fn broker_port_extracts_from_bind() {
        assert_eq!(broker_port("0.0.0.0:8849"), "8849");
        assert_eq!(broker_port("0.0.0.0:1234"), "1234");
        assert_eq!(broker_port("localhost"), "localhost");
    }

    #[test]
    fn broker_endpoint_from_url_no_override_is_unchanged() {
        // The no-op guarantee: default cfg + podman driver must produce the exact same
        // endpoint the old code did, byte for byte.
        let cfg = BrokerCfg::default();
        let url = resolve_broker_url(&cfg, "host.containers.internal");
        assert_eq!(
            broker_endpoint_from_url(&url).unwrap(),
            "host.containers.internal:8849:full"
        );
        // Same for the kubernetes driver host.
        let url = resolve_broker_url(&cfg, "host.openshell.internal");
        assert_eq!(
            broker_endpoint_from_url(&url).unwrap(),
            "host.openshell.internal:8849:full"
        );
    }

    #[test]
    fn broker_endpoint_from_url_explicit_override_with_port() {
        assert_eq!(
            broker_endpoint_from_url("http://broker.example:9000/mcp").unwrap(),
            "broker.example:9000:full"
        );
    }

    #[test]
    fn broker_endpoint_from_url_https_no_port() {
        assert_eq!(
            broker_endpoint_from_url("https://broker.example/mcp").unwrap(),
            "broker.example:443:full"
        );
    }

    #[test]
    fn broker_endpoint_from_url_http_no_port() {
        assert_eq!(
            broker_endpoint_from_url("http://broker.example/mcp").unwrap(),
            "broker.example:80:full"
        );
    }

    #[test]
    fn broker_endpoint_from_url_malformed() {
        assert!(
            broker_endpoint_from_url("not-a-url").is_err(),
            "a URL without a scheme must fail, not panic"
        );
        assert!(
            broker_endpoint_from_url("http://").is_err(),
            "an empty authority must fail"
        );
    }

    #[test]
    fn broker_endpoint_from_url_rejects_a_typoed_port() {
        // A colon on an unbracketed authority is always a port separator. Falling back to the
        // scheme default here would emit `broker.example:abc:80:full`, a four-field endpoint the
        // sandbox's policy engine would then choke on, far from the typo that caused it.
        for bad in [
            "http://broker.example:abc/mcp",
            "http://broker.example:/mcp",
            "http://broker.example:99999/mcp",
        ] {
            assert!(
                broker_endpoint_from_url(bad).is_err(),
                "{bad} must be rejected, not silently defaulted"
            );
        }
    }

    #[test]
    fn broker_endpoint_from_url_handles_bracketed_ipv6() {
        // The bracket case is the one place a colon is not a port separator.
        assert_eq!(
            broker_endpoint_from_url("http://[::1]:8849/mcp").unwrap(),
            "[::1]:8849:full"
        );
        assert_eq!(
            broker_endpoint_from_url("https://[::1]/mcp").unwrap(),
            "[::1]:443:full"
        );
    }

    #[test]
    fn broker_cfg_url_none_parses_from_toml() {
        // A manifest that omits `url` from `[agent.broker]` must parse with url = None.
        let toml_str = r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "test"
            [agent.broker]
            enabled = true
            bin = "my-broker"
            [judge]
            measure_cmd = "./m"
            direction = "higher"
            objective = "x"
        "#;
        let m: Manifest = toml::from_str(toml_str).expect("parses");
        assert!(m.agent.broker.url.is_none());
        assert!(m.agent.broker.enabled);
    }

    #[test]
    fn broker_cfg_url_some_parses_from_toml() {
        // A manifest that sets `url` explicitly in `[agent.broker]`.
        let toml_str = r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "test"
            [agent.broker]
            enabled = true
            bin = "my-broker"
            url = "http://custom:9999/mcp"
            [judge]
            measure_cmd = "./m"
            direction = "higher"
            objective = "x"
        "#;
        let m: Manifest = toml::from_str(toml_str).expect("parses");
        assert_eq!(
            m.agent.broker.url.as_deref(),
            Some("http://custom:9999/mcp")
        );
    }
}

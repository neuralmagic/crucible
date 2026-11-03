//! One egress endpoint in OpenShell's `host:port[:access[:protocol[:enforcement]]]` form, parsed
//! once. The policy allowlist, the gateway rule builder, the disclosure, and the broker URL all
//! read and write this type rather than splitting the string themselves.

use std::fmt;
use std::str::FromStr;

/// What the proxy lets through to the endpoint. Absent, OpenShell applies its own default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
    Full,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::ReadOnly => "read-only",
            Access::ReadWrite => "read-write",
            Access::Full => "full",
        }
    }

    fn parse(raw: &str) -> Option<Option<Self>> {
        match raw {
            "" => Some(None),
            "read-only" => Some(Some(Access::ReadOnly)),
            "read-write" => Some(Some(Access::ReadWrite)),
            "full" => Some(Some(Access::Full)),
            _ => None,
        }
    }
}

/// The L7 protocol the proxy inspects. Absent, the endpoint is an L4 tunnel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Rest,
    Websocket,
    Sql,
    Mcp,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Rest => "rest",
            Protocol::Websocket => "websocket",
            Protocol::Sql => "sql",
            Protocol::Mcp => "mcp",
        }
    }

    fn parse(raw: &str) -> Option<Option<Self>> {
        match raw {
            "" => Some(None),
            "rest" => Some(Some(Protocol::Rest)),
            "websocket" => Some(Some(Protocol::Websocket)),
            "sql" => Some(Some(Protocol::Sql)),
            "mcp" => Some(Some(Protocol::Mcp)),
            _ => None,
        }
    }
}

/// Whether an L7 violation is refused or only logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enforcement {
    Enforce,
    Audit,
}

impl Enforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Enforcement::Enforce => "enforce",
            Enforcement::Audit => "audit",
        }
    }

    fn parse(raw: &str) -> Option<Option<Self>> {
        match raw {
            "" => Some(None),
            "enforce" => Some(Some(Enforcement::Enforce)),
            "audit" => Some(Some(Enforcement::Audit)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// A hostname, a `*`/`**` pattern, or a bracketed IPv6 literal.
    pub host: String,
    pub port: u16,
    pub access: Option<Access>,
    pub protocol: Option<Protocol>,
    pub enforcement: Option<Enforcement>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
#[error("endpoint '{spec}' {problem}")]
pub struct EndpointSpecError {
    pub spec: String,
    pub problem: EndpointProblem,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EndpointProblem {
    #[error("must be host:port[:access[:protocol[:enforcement]]]")]
    Shape,
    #[error("has an invalid host segment")]
    Host,
    #[error("has a non-numeric port")]
    NonNumericPort,
    #[error("port must be in 1..=65535")]
    PortRange,
    #[error("access must be read-only, read-write, or full")]
    Access,
    #[error("protocol must be rest, websocket, sql, or mcp")]
    Protocol,
    #[error("an mcp endpoint's access must be full (every known MCP method) or omitted")]
    McpAccess,
    #[error("enforcement must be enforce or audit")]
    Enforcement,
    #[error("cannot set enforcement without a protocol")]
    EnforcementWithoutProtocol,
}

impl Endpoint {
    /// An L4 entry: `host:port:access`.
    pub fn l4(host: impl Into<String>, port: u16, access: Access) -> Self {
        Endpoint {
            host: host.into(),
            port,
            access: Some(access),
            protocol: None,
            enforcement: None,
        }
    }

    /// The host patterns a deny naming this endpoint denies: the host itself plus everything
    /// under it, unless the host is already a wildcard, which names only what it matches.
    fn denied_patterns(&self) -> Vec<String> {
        if self.host.contains('*') {
            vec![self.host.clone()]
        } else {
            vec![self.host.clone(), format!("**.{}", self.host)]
        }
    }

    /// Whether every host `allow` admits is one this deny names, so the entry can be dropped
    /// whole. A literal deny subsumes any pattern whose every match ends in `.<host>`, wildcard
    /// shapes included; a wildcard deny drops a literal entry but never a wildcard one it does
    /// not spell identically.
    pub fn covers_host_of(&self, allow: &Endpoint) -> bool {
        if self.host.eq_ignore_ascii_case(&allow.host) {
            return true;
        }
        if self.host.contains('*') {
            return !allow.host.contains('*')
                && openshell_core::host_pattern::host_matches(&self.host, &allow.host)
                    .unwrap_or(false);
        }
        let suffix = format!(".{}", self.host.to_ascii_lowercase());
        allow.host.len() > suffix.len() && allow.host.to_ascii_lowercase().ends_with(&suffix)
    }

    /// Whether `allow` admits any host this deny names, whether or not it can be dropped whole.
    pub fn overlaps_host_of(&self, allow: &Endpoint) -> bool {
        self.denied_patterns().iter().any(|pattern| {
            openshell_core::host_pattern::host_patterns_overlap(pattern, &allow.host)
                .unwrap_or(false)
        })
    }
}

impl FromStr for Endpoint {
    type Err = EndpointSpecError;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let reject = |problem| EndpointSpecError {
            spec: spec.to_owned(),
            problem,
        };
        // A bracketed IPv6 literal keeps its colons inside the brackets.
        let (host, rest) = match spec.strip_prefix('[') {
            Some(after) => {
                let close = after
                    .find(']')
                    .ok_or_else(|| reject(EndpointProblem::Host))?;
                let rest = after[close + 1..]
                    .strip_prefix(':')
                    .ok_or_else(|| reject(EndpointProblem::Shape))?;
                (format!("[{}]", &after[..close]), rest)
            }
            None => {
                let (host, rest) = spec
                    .split_once(':')
                    .ok_or_else(|| reject(EndpointProblem::Shape))?;
                (host.trim().to_string(), rest)
            }
        };
        let fields: Vec<&str> = rest.split(':').map(str::trim).collect();
        if fields.len() > 4 {
            return Err(reject(EndpointProblem::Shape));
        }
        if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
            return Err(reject(EndpointProblem::Host));
        }
        let port: u32 = fields[0]
            .parse()
            .map_err(|_| reject(EndpointProblem::NonNumericPort))?;
        let port = u16::try_from(port)
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| reject(EndpointProblem::PortRange))?;
        let field = |i: usize| fields.get(i).copied().unwrap_or("");
        let access = Access::parse(field(1)).ok_or_else(|| reject(EndpointProblem::Access))?;
        let protocol =
            Protocol::parse(field(2)).ok_or_else(|| reject(EndpointProblem::Protocol))?;
        let enforcement =
            Enforcement::parse(field(3)).ok_or_else(|| reject(EndpointProblem::Enforcement))?;
        if enforcement.is_some() && protocol.is_none() {
            return Err(reject(EndpointProblem::EnforcementWithoutProtocol));
        }
        if protocol == Some(Protocol::Mcp) && !matches!(access, None | Some(Access::Full)) {
            return Err(reject(EndpointProblem::McpAccess));
        }
        Ok(Endpoint {
            host,
            port,
            access,
            protocol,
            enforcement,
        })
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)?;
        let access = self.access.map(Access::as_str).unwrap_or("");
        let protocol = self.protocol.map(Protocol::as_str);
        let enforcement = self.enforcement.map(Enforcement::as_str);
        match (protocol, enforcement) {
            (None, _) => {
                if !access.is_empty() {
                    write!(f, ":{access}")?;
                }
            }
            (Some(protocol), None) => write!(f, ":{access}:{protocol}")?,
            (Some(protocol), Some(enforcement)) => write!(f, ":{access}:{protocol}:{enforcement}")?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::openshell::endpoint::*;

    #[test]
    fn every_segment_round_trips_through_display() {
        for spec in [
            "github.com:443",
            "github.com:443:full",
            "api.example.com:443:read-write:rest:enforce",
            "h:8850:full:mcp",
            "h:8850::mcp",
            "[::1]:8849:full",
            "*.github.com:443:read-only",
        ] {
            let ep: Endpoint = spec.parse().unwrap_or_else(|e| panic!("{spec}: {e}"));
            assert_eq!(ep.to_string(), spec);
        }
    }

    #[test]
    fn a_bracketed_ipv6_literal_keeps_its_colons() {
        let ep: Endpoint = "[fe80::1]:443:full".parse().unwrap();
        assert_eq!(ep.host, "[fe80::1]");
        assert_eq!(ep.port, 443);
        assert_eq!(ep.access, Some(Access::Full));
    }

    #[test]
    fn each_malformed_segment_names_its_problem() {
        let problem = |spec: &str| spec.parse::<Endpoint>().unwrap_err().problem;
        assert_eq!(problem("host-only"), EndpointProblem::Shape);
        assert_eq!(problem("host:notaport"), EndpointProblem::NonNumericPort);
        assert_eq!(problem("host:70000"), EndpointProblem::PortRange);
        assert_eq!(problem("host:0"), EndpointProblem::PortRange);
        assert_eq!(problem(":443"), EndpointProblem::Host);
        assert_eq!(problem("ho st:443"), EndpointProblem::Host);
        assert_eq!(problem("host:443:write-ish"), EndpointProblem::Access);
        assert_eq!(problem("host:443:full:ftp"), EndpointProblem::Protocol);
        assert_eq!(
            problem("host:443:full::enforce"),
            EndpointProblem::EnforcementWithoutProtocol
        );
        assert_eq!(
            problem("host:443:full:rest:maybe"),
            EndpointProblem::Enforcement
        );
        assert_eq!(problem("h:8850:read-only:mcp"), EndpointProblem::McpAccess);
        assert_eq!(
            problem("h:443:full:rest:enforce:extra"),
            EndpointProblem::Shape
        );
    }

    #[test]
    fn a_deny_covers_its_subdomains_and_only_the_wildcards_it_spells() {
        let parse = |s: &str| s.parse::<Endpoint>().unwrap();
        let github = parse("github.com:443:full");
        assert!(github.covers_host_of(&parse("api.github.com:443:full")));
        assert!(github.covers_host_of(&parse("*.github.com:443:full")));
        assert!(!github.covers_host_of(&parse("notgithub.com:443:full")));
        let wild = parse("*.github.com:443:full");
        assert!(wild.covers_host_of(&parse("api.github.com:443:full")));
        assert!(!wild.covers_host_of(&parse("**.github.com:443:full")));
        assert!(wild.overlaps_host_of(&parse("**.github.com:443:full")));
    }
}

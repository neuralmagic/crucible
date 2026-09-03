//! Resolve the OpenShell egress allowlist pushed via the gateway's `UpdateConfig` RPC
//! ([`crate::openshell::grpc::Gateway::update_policy_wait`]). The domain's extra
//! endpoints/binaries live in the manifest (`[agent.openshell]`,
//! [`crate::manifest::OpenshellCfg`]), so the policy is owned by the domain.
//!
//! The sandbox is deny-by-default; these are the endpoints the agent's tooling is allowed
//! to reach and the binaries allowed to open egress. Defaults cover the common case
//! (Vertex, the forges, PyPI, Anthropic); a domain appends cluster/registry/HF endpoints.
//!
//! A domain that sets `inherit_defaults = false` drops the built-ins entirely and the
//! resolved allowlist is exactly what its manifest lists. That is the air-gap / private-registry
//! path. To *subtract* specific defaults (notably GitHub, which an agent being scored on an
//! upstream issue can otherwise read the fix from) without losing the rest of the built-ins, use
//! `deny_endpoints`/`deny_binaries`, applied last so deny beats both an inherited default and an
//! appended extra. `deny_endpoints` is host-scoped, not exact-string: denying
//! `github.com:443:full` also removes `*.github.com:443:full`, so one entry seals the domain.
//! See [`resolve_endpoints`] for the full rule. `deny_binaries` stays exact-string, a binary path
//! has no domain structure.

use crate::manifest::{Harness, OpenshellCfg};

/// Built-in egress endpoints in openshell's `host:port:access[:proto[:enforcement]]` form.
/// No protocol is given, so these are L4-only (CONNECT tunneling): `protocol=rest` would
/// enable L7 inspection, which blocks the CONNECT that Vertex streaming/gRPC clients use.
/// The Vertex hosts a claude turn's access token is scoped to. The `google-cloud` provider profile
/// declares no endpoints, so its credential is only delivered to a sandbox when a policy endpoint
/// names the provider: unbound, openshell fails closed and the sandbox's metadata emulator answers
/// 503 with no token. Must stay a subset of the aiplatform hosts in `DEFAULT_ENDPOINTS`, matched
/// there by exact host and port.
///
/// A region other than `global` moves the API to its own apex domain, `<region>-aiplatform.
/// googleapis.com` for a region and `aiplatform.<multi-region>.rep.googleapis.com` for `us`/`eu`,
/// neither of which is a subdomain of the global host.
pub const VERTEX_CREDENTIAL_HOSTS: &[&str] = &[
    "aiplatform.googleapis.com",
    "*.aiplatform.googleapis.com",
    "*-aiplatform.googleapis.com",
    "aiplatform.us.rep.googleapis.com",
    "aiplatform.eu.rep.googleapis.com",
];

pub const DEFAULT_ENDPOINTS: &[&str] = &[
    "github.com:443:full",
    "*.github.com:443:full",
    "gitlab.com:443:full",
    "*.gitlab.com:443:full",
    "pypi.org:443:read-only",
    "files.pythonhosted.org:443:read-only",
    "aiplatform.googleapis.com:443:read-write",
    "*.aiplatform.googleapis.com:443:read-write",
    "*-aiplatform.googleapis.com:443:read-write",
    "aiplatform.us.rep.googleapis.com:443:read-write",
    "aiplatform.eu.rep.googleapis.com:443:read-write",
    "oauth2.googleapis.com:443:read-write",
    "api.anthropic.com:443:read-write",
];

/// Egress hosts a codex turn needs on top of the shared defaults. `full` (raw L4 tunnel, like
/// github's default) rather than `read-write`: the proxy applies protocol handling to read-write
/// endpoints, and codex's streaming connection to the ChatGPT backend dies mid-stream through it.
pub const CODEX_ENDPOINTS: &[&str] = &[
    "chatgpt.com:443:full",
    "auth.openai.com:443:full",
    "api.openai.com:443:full",
    "ab.chatgpt.com:443:full",
];

/// The egress allowlist built-ins for `harness`: the shared defaults, plus the model backend's
/// own hosts for a harness that does not talk to Vertex. Per-harness so a claude turn's allowlist
/// never grows the OpenAI hosts a codex turn needs.
pub fn default_endpoints(harness: Harness) -> Vec<&'static str> {
    let mut out = DEFAULT_ENDPOINTS.to_vec();
    if harness == Harness::Codex {
        out.extend_from_slice(CODEX_ENDPOINTS);
    }
    out
}

/// The Vertex keys a manifest-less turn (`rank-grounded`, scope-propose) relays from its own
/// process env into the agent env: the claude switches plus every alias `run::vertex_config`
/// honors. A domain loop gets these from `[agent].env` (the manifest validates them); a bare
/// `git clone` turn has no manifest, so the turn pod's plain env (the deploy profile's `[env]`)
/// carries them and this relay is the explicit bridge. `vertex_config`/`env_script` stay
/// manifest-only, they never read the process env themselves.
pub const VERTEX_RELAY_KEYS: &[&str] = &[
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "GCP_PROJECT_ID",
    "VERTEX_LOCATION",
];

/// The identity a run's commits are attributed to, set on the pod by the controller when the run
/// pushes as its GitHub App. Not a credential: it names an author, and the agent is the one that
/// commits. The env spelling is what makes it win — it outranks `user.name`/`user.email` from a
/// config file and from the `-c` overrides a pack's `setup_cmd` passes.
pub const IDENTITY_RELAY_KEYS: &[&str] = &[
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
];

/// The harness's built-in endpoints (`defaults`, see
/// `HarnessRuntime::default_endpoints`) plus the domain's extras, de-duplicated, order
/// preserved, then with `deny_endpoints` subtracted. With `inherit_defaults = false` the built-ins
/// are dropped and only the domain's are allowed.
///
/// # Deny semantics
///
/// A deny wins over any allow that would admit the denied `host:port`. Both sides are parsed as
/// `host:port:...`; the port must match exactly (denying `:443` leaves `:8443` alone) and the
/// access/protocol/enforcement tail is ignored, so a deny removes an entry at any access level.
/// Hosts are compared as domain scopes with openshell's own DNS-label matcher (`*` inside one
/// label, `**` across several, case-insensitive):
///
/// - deny `github.com` removes `github.com`, `*.github.com`, and `api.github.com`,
/// - deny `*.github.com` removes `*.github.com` and `api.github.com` but leaves the apex
///   `github.com`,
/// - deny `api.github.com` removes only `api.github.com`, and leaves `*.github.com`, which still
///   admits the denied host. Nothing narrower can be subtracted from a broader wildcard, so that
///   residue is reported by `crucible check` rather than silently honored here.
///
/// A deny entry with no parseable `host:port` prefix falls back to exact-string equality.
///
/// `broker_endpoint`, when `Some`, is the engine-resolved broker `host:port:access` entry. It is
/// appended **after** the deny subtraction and regardless of `inherit_defaults`, because the
/// broker is engine plumbing the domain opted into by enabling `[agent.broker]`, not a built-in
/// the domain can subtract. A broker-less domain (no broker_endpoint) with
/// `inherit_defaults = false` still resolves to exactly what its manifest lists.
pub fn resolve_endpoints(
    cfg: &OpenshellCfg,
    defaults: &[&str],
    broker_endpoint: Option<&str>,
) -> Vec<String> {
    let merged = merge(inherited(cfg, defaults), &cfg.endpoints);
    let mut out = subtract_endpoints(merged, &cfg.deny_endpoints);
    if let Some(ep) = broker_endpoint
        && !out.iter().any(|seen| seen == ep)
    {
        out.push(ep.to_string());
    }
    out
}

/// The harness's agent-CLI binaries (`defaults`, see `HarnessRuntime::default_binaries`)
/// plus the domain's extras, de-duplicated, order preserved, then with `deny_binaries`
/// subtracted. With `inherit_defaults = false` an unlisted agent CLI gets no network at all.
pub fn resolve_binaries(cfg: &OpenshellCfg, defaults: &[&str]) -> Vec<String> {
    subtract(
        merge(inherited(cfg, defaults), &cfg.binaries),
        &cfg.deny_binaries,
    )
}

/// The built-ins the domain opted into, or nothing when it opted out.
fn inherited<'a>(cfg: &OpenshellCfg, defaults: &'a [&'a str]) -> &'a [&'a str] {
    if cfg.inherit_defaults { defaults } else { &[] }
}

/// Append `extra` to `defaults`, dropping duplicates while preserving first-seen order.
fn merge(defaults: &[&str], extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = defaults.iter().map(|s| s.to_string()).collect();
    for item in extra {
        if !out.iter().any(|seen| seen == item) {
            out.push(item.clone());
        }
    }
    out
}

/// Drop any entry in `list` that exact-string-matches one in `deny`. A deny of an entry that
/// isn't present is a no-op.
fn subtract(list: Vec<String>, deny: &[String]) -> Vec<String> {
    list.into_iter()
        .filter(|item| !deny.iter().any(|d| d == item))
        .collect()
}

/// Drop any endpoint in `list` that a `deny` entry covers, per the rule documented on
/// [`resolve_endpoints`].
fn subtract_endpoints(list: Vec<String>, deny: &[String]) -> Vec<String> {
    list.into_iter()
        .filter(|item| !deny.iter().any(|d| denies(d, item)))
        .collect()
}

/// Whether the deny entry `deny` removes the allowlist entry `allow`.
fn denies(deny: &str, allow: &str) -> bool {
    match (Endpoint::parse(deny), Endpoint::parse(allow)) {
        (Some(d), Some(a)) => d.port == a.port && d.covers_host_of(&a),
        _ => deny == allow,
    }
}

/// A deny entry that survived subtraction only because a broader allowlist entry still admits the
/// host:port it names.
pub struct ShadowedDeny {
    pub deny: String,
    pub allow: String,
}

/// Deny entries whose `host:port` a surviving `resolved` entry still admits, each paired with the
/// entry that admits it. Non-empty only when the deny names something strictly narrower than a
/// surviving wildcard (deny `api.github.com:443:full` against an allowed `*.github.com:443:full`),
/// which subtraction cannot carve a hole in.
pub fn shadowed_denies(resolved: &[String], deny: &[String]) -> Vec<ShadowedDeny> {
    let mut out = Vec::new();
    for d in deny {
        let Some(dep) = Endpoint::parse(d) else {
            continue;
        };
        for a in resolved {
            let Some(aep) = Endpoint::parse(a) else {
                continue;
            };
            if dep.port == aep.port && !dep.covers_host_of(&aep) && dep.overlaps_host_of(&aep) {
                out.push(ShadowedDeny {
                    deny: d.clone(),
                    allow: a.clone(),
                });
            }
        }
    }
    out
}

/// The `host:port` prefix of an openshell `host:port:access[:proto[:enforcement]]` entry. The tail
/// is dropped: a deny is about reachability of a host:port, at whatever access level.
///
/// Host comparisons go through openshell's own DNS-label matcher
/// ([`openshell_core::host_pattern`]): `*` stays inside a label, `**` spans several.
struct Endpoint<'a> {
    host: &'a str,
    port: &'a str,
}

impl<'a> Endpoint<'a> {
    fn parse(entry: &'a str) -> Option<Self> {
        let mut parts = entry.split(':');
        let host = parts.next().filter(|h| !h.is_empty())?;
        let port = parts.next().filter(|p| !p.is_empty())?;
        Some(Self { host, port })
    }

    /// The host patterns a deny naming this endpoint denies: the host itself plus everything under
    /// it, unless the host is already a wildcard, which names only what it matches.
    fn denied_patterns(&self) -> Vec<String> {
        if self.host.contains('*') {
            vec![self.host.to_string()]
        } else {
            vec![self.host.to_string(), format!("**.{}", self.host)]
        }
    }

    /// Whether every host `allow` admits is one this deny names, so the entry can be dropped whole.
    /// A literal deny subsumes any pattern whose every match ends in `.<host>`, wildcard shapes
    /// included; a wildcard deny drops a literal entry but never a wildcard one it does not spell
    /// identically.
    fn covers_host_of(&self, allow: &Endpoint<'_>) -> bool {
        if self.host.eq_ignore_ascii_case(allow.host) {
            return true;
        }
        if self.host.contains('*') {
            return !allow.host.contains('*')
                && openshell_core::host_pattern::host_matches(self.host, allow.host)
                    .unwrap_or(false);
        }
        let suffix = format!(".{}", self.host.to_ascii_lowercase());
        allow.host.len() > suffix.len() && allow.host.to_ascii_lowercase().ends_with(&suffix)
    }

    /// Whether `allow` admits any host this deny names, whether or not it can be dropped whole.
    fn overlaps_host_of(&self, allow: &Endpoint<'_>) -> bool {
        self.denied_patterns().iter().any(|pattern| {
            openshell_core::host_pattern::host_patterns_overlap(pattern, allow.host)
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claude harness's binary defaults, what every pre-harness-boundary test resolved
    /// against.
    const DEFAULT_BINARIES: &[&str] = &["/usr/local/bin/claude", "/usr/local/bin/opencode"];

    fn cfg(endpoints: &[&str], binaries: &[&str]) -> OpenshellCfg {
        OpenshellCfg {
            endpoints: endpoints.iter().map(|s| s.to_string()).collect(),
            binaries: binaries.iter().map(|s| s.to_string()).collect(),
            ..OpenshellCfg::default()
        }
    }

    /// The air-gap / anti-contamination config: opt out, then list exactly what's allowed.
    fn sealed(endpoints: &[&str], binaries: &[&str]) -> OpenshellCfg {
        OpenshellCfg {
            inherit_defaults: false,
            ..cfg(endpoints, binaries)
        }
    }

    #[test]
    fn defaults_when_no_extras() {
        let c = OpenshellCfg::default();
        assert_eq!(
            resolve_endpoints(&c, DEFAULT_ENDPOINTS, None).len(),
            DEFAULT_ENDPOINTS.len()
        );
        assert_eq!(resolve_binaries(&c, DEFAULT_BINARIES), DEFAULT_BINARIES);
    }

    #[test]
    fn extras_append_after_defaults() {
        let c = cfg(
            &["api.internal.example:443:read-write"],
            &["/usr/local/bin/kubectl"],
        );
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert_eq!(eps.first().unwrap(), DEFAULT_ENDPOINTS[0], "defaults first");
        assert_eq!(eps.last().unwrap(), "api.internal.example:443:read-write");
        assert!(
            resolve_binaries(&c, DEFAULT_BINARIES).contains(&"/usr/local/bin/kubectl".to_string())
        );
    }

    #[test]
    fn opting_out_replaces_the_defaults_outright() {
        let c = sealed(
            &["registry.internal:443:read-only"],
            &["/usr/local/bin/claude"],
        );
        assert_eq!(
            resolve_endpoints(&c, DEFAULT_ENDPOINTS, None),
            ["registry.internal:443:read-only"]
        );
        assert_eq!(
            resolve_binaries(&c, DEFAULT_BINARIES),
            ["/usr/local/bin/claude"]
        );
    }

    #[test]
    fn opting_out_subtracts_github() {
        // The contamination guard: an agent scored on an upstream issue must not be able to
        // read the upstream fix. Appending can never remove a default; opting out can (and, as
        // of #192, `deny_endpoints` can too without losing the rest of the built-ins).
        let appended = cfg(&["registry.internal:443:read-only"], &[]);
        assert!(
            resolve_endpoints(&appended, DEFAULT_ENDPOINTS, None)
                .iter()
                .any(|e| e.contains("github.com"))
        );

        let c = sealed(
            &["registry.internal:443:read-only"],
            &["/usr/local/bin/claude"],
        );
        assert!(
            !resolve_endpoints(&c, DEFAULT_ENDPOINTS, None)
                .iter()
                .any(|e| e.contains("github.com")),
            "github must not survive an opt-out"
        );
    }

    #[test]
    fn opting_out_with_an_empty_table_denies_everything() {
        // A total air-gap is expressible: no endpoints, and no binary may open a socket.
        // The broker is NOT enabled here, so no broker_endpoint is passed.
        let c = sealed(&[], &[]);
        assert!(resolve_endpoints(&c, DEFAULT_ENDPOINTS, None).is_empty());
        assert!(resolve_binaries(&c, DEFAULT_BINARIES).is_empty());
    }

    #[test]
    fn an_absent_openshell_table_still_inherits() {
        // `#[serde(default)]` on the field must not hand us `inherit_defaults = false`.
        let c = OpenshellCfg::default();
        assert!(c.inherit_defaults);
        assert_eq!(
            resolve_endpoints(&c, DEFAULT_ENDPOINTS, None).len(),
            DEFAULT_ENDPOINTS.len()
        );
    }

    #[test]
    fn duplicate_extras_are_dropped() {
        // An extra that repeats a default must not appear twice.
        let c = cfg(&["github.com:443:full", "github.com:443:full"], &[]);
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert_eq!(
            eps.iter().filter(|e| *e == "github.com:443:full").count(),
            1,
            "deduped: {eps:?}"
        );
        assert_eq!(
            eps.len(),
            DEFAULT_ENDPOINTS.len(),
            "no net growth from a dup"
        );
    }

    // --- broker endpoint auto-append ---

    #[test]
    fn broker_endpoint_appended_with_defaults() {
        // A domain that inherits defaults AND enables the broker gets the broker endpoint
        // auto-appended at the end, without duplicates.
        let c = OpenshellCfg::default();
        let ep = "host.containers.internal:8849:full";
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, Some(ep));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() + 1);
        assert_eq!(eps.last().unwrap(), ep);
    }

    #[test]
    fn broker_endpoint_appended_after_opt_out() {
        // A domain that opts out of defaults but enables the broker still gets the broker
        // endpoint: the broker is engine plumbing, not a domain-subtractable built-in.
        let c = sealed(&[], &[]);
        let ep = "host.containers.internal:8849:full";
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, Some(ep));
        assert_eq!(eps, vec![ep]);
    }

    #[test]
    fn broker_endpoint_deduped_when_already_listed() {
        // If a (legacy) manifest still hand-lists the broker endpoint AND the engine passes
        // it, it must not appear twice.
        let ep = "host.containers.internal:8849:full";
        let c = cfg(&[ep], &[]);
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, Some(ep));
        assert_eq!(
            eps.iter().filter(|e| *e == ep).count(),
            1,
            "deduped: {eps:?}"
        );
    }

    #[test]
    fn kubernetes_broker_endpoint_appended() {
        // Under the kubernetes driver, the broker host is `host.openshell.internal`.
        let c = OpenshellCfg::default();
        let ep = "host.openshell.internal:8849:full";
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, Some(ep));
        assert!(eps.contains(&ep.to_string()));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() + 1);
    }

    #[test]
    fn no_broker_no_append() {
        // A broker-less domain: None broker_endpoint, nothing extra appended.
        let c = OpenshellCfg::default();
        assert_eq!(
            resolve_endpoints(&c, DEFAULT_ENDPOINTS, None).len(),
            DEFAULT_ENDPOINTS.len(),
            "no broker means no extra endpoint"
        );
    }

    // --- deny_endpoints / deny_binaries (#192) ---

    #[test]
    fn deny_beats_an_inherited_default() {
        // The contamination guard: denying the apex seals the domain, wildcard sibling included,
        // so an in-loop agent cannot read the upstream fix off `api.github.com`.
        let c = OpenshellCfg {
            deny_endpoints: vec!["github.com:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(
            !eps.iter().any(|e| e.contains("github.com")),
            "apex deny must take the wildcard too: {eps:?}"
        );
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() - 2);
    }

    #[test]
    fn deny_both_github_entries_is_idempotent() {
        // Listing both the apex and the wildcard must resolve to the same sealed allowlist
        // rather than over-subtracting.
        let c = OpenshellCfg {
            deny_endpoints: vec![
                "github.com:443:full".to_string(),
                "*.github.com:443:full".to_string(),
            ],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(!eps.iter().any(|e| e.contains("github.com")));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() - 2);
    }

    #[test]
    fn deny_of_a_wildcard_leaves_the_apex() {
        let c = OpenshellCfg {
            deny_endpoints: vec!["*.github.com:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(eps.contains(&"github.com:443:full".to_string()));
        assert!(!eps.contains(&"*.github.com:443:full".to_string()));
        assert!(
            shadowed_denies(&eps, &c.deny_endpoints).is_empty(),
            "the surviving apex does not admit any host `*.github.com` names: {eps:?}"
        );
    }

    #[test]
    fn deny_matches_at_any_access_level() {
        let c = OpenshellCfg {
            endpoints: vec!["registry.internal:443:read-write:rest".to_string()],
            deny_endpoints: vec!["registry.internal:443:read-only".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(!eps.iter().any(|e| e.starts_with("registry.internal:")));
    }

    #[test]
    fn deny_is_port_exact() {
        let c = OpenshellCfg {
            endpoints: vec![
                "registry.internal:8443:full".to_string(),
                "*.registry.internal:8443:full".to_string(),
                "registry.internal:443:full".to_string(),
            ],
            deny_endpoints: vec!["registry.internal:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(eps.contains(&"registry.internal:8443:full".to_string()));
        assert!(eps.contains(&"*.registry.internal:8443:full".to_string()));
        assert!(!eps.contains(&"registry.internal:443:full".to_string()));
    }

    #[test]
    fn deny_takes_a_deeper_host_but_not_a_sibling() {
        let c = OpenshellCfg {
            endpoints: vec![
                "api.github.com:443:full".to_string(),
                "raw.githubusercontent.com:443:full".to_string(),
            ],
            deny_endpoints: vec!["github.com:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(!eps.contains(&"api.github.com:443:full".to_string()));
        assert!(
            eps.contains(&"raw.githubusercontent.com:443:full".to_string()),
            "a host that merely shares a suffix substring is a different domain: {eps:?}"
        );
    }

    #[test]
    fn deny_host_matching_is_case_insensitive() {
        let c = OpenshellCfg {
            deny_endpoints: vec!["GitHub.COM:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(!eps.iter().any(|e| e.to_lowercase().contains("github.com")));
    }

    #[test]
    fn a_narrow_deny_is_reported_as_shadowed_not_honored() {
        let c = OpenshellCfg {
            deny_endpoints: vec!["api.github.com:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(eps.contains(&"*.github.com:443:full".to_string()));
        let shadowed = shadowed_denies(&eps, &c.deny_endpoints);
        assert_eq!(shadowed.len(), 1);
        assert_eq!(shadowed[0].deny, "api.github.com:443:full");
        assert_eq!(shadowed[0].allow, "*.github.com:443:full");
    }

    #[test]
    fn an_effective_deny_is_not_reported_as_shadowed() {
        let c = OpenshellCfg {
            deny_endpoints: vec!["github.com:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(shadowed_denies(&eps, &c.deny_endpoints).is_empty());
    }

    #[test]
    fn a_deny_shadowed_only_on_another_port_is_not_reported() {
        let c = OpenshellCfg {
            endpoints: vec!["*.registry.internal:8443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(
            shadowed_denies(&eps, &["one.registry.internal:443:full".to_string()]).is_empty(),
            "a :443 deny is not shadowed by an :8443 allow"
        );
        assert_eq!(
            shadowed_denies(&eps, &["one.registry.internal:8443:full".to_string()]).len(),
            1
        );
    }

    #[test]
    fn deny_beats_an_appended_extra() {
        let c = OpenshellCfg {
            endpoints: vec!["registry.internal:443:read-only".to_string()],
            deny_endpoints: vec!["registry.internal:443:read-only".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, None);
        assert!(!eps.contains(&"registry.internal:443:read-only".to_string()));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len());
    }

    #[test]
    fn deny_of_an_absent_entry_is_a_no_op() {
        let c = OpenshellCfg {
            deny_endpoints: vec!["nowhere.example:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        assert_eq!(
            resolve_endpoints(&c, DEFAULT_ENDPOINTS, None).len(),
            DEFAULT_ENDPOINTS.len()
        );

        let cb = OpenshellCfg {
            deny_binaries: vec!["/usr/local/bin/nope".to_string()],
            ..OpenshellCfg::default()
        };
        assert_eq!(resolve_binaries(&cb, DEFAULT_BINARIES), DEFAULT_BINARIES);
    }

    #[test]
    fn deny_beats_default_but_broker_endpoint_still_appended() {
        // deny_endpoints only subtracts from defaults/extras; the broker entry is appended
        // after, same as the existing opt-out behavior.
        let c = OpenshellCfg {
            deny_endpoints: vec!["github.com:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let ep = "host.containers.internal:8849:full";
        let eps = resolve_endpoints(&c, DEFAULT_ENDPOINTS, Some(ep));
        assert!(!eps.iter().any(|e| e.contains("github.com")));
        assert_eq!(eps.last().unwrap(), ep);
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() - 1);
    }

    #[test]
    fn deny_beats_an_inherited_default_binary() {
        let c = OpenshellCfg {
            deny_binaries: vec!["/usr/local/bin/opencode".to_string()],
            ..OpenshellCfg::default()
        };
        assert_eq!(
            resolve_binaries(&c, DEFAULT_BINARIES),
            ["/usr/local/bin/claude"]
        );
    }
    #[test]
    fn the_credential_hosts_cover_every_endpoint_the_gateway_builds() {
        // openshell-server builds `global` -> aiplatform.googleapis.com, `us`/`eu` ->
        // aiplatform.<mr>.rep.googleapis.com, anything else -> <region>-aiplatform.googleapis.com.
        let covered = |host: &str| {
            VERTEX_CREDENTIAL_HOSTS
                .iter()
                .any(|p| match p.strip_prefix('*') {
                    Some(suffix) => host.ends_with(suffix) && host.len() > suffix.len(),
                    None => *p == host,
                })
        };
        for region in [
            "global",
            "us",
            "eu",
            "us-east5",
            "us-central1",
            "europe-west1",
        ] {
            let host = match region {
                "global" => "aiplatform.googleapis.com".to_string(),
                "us" | "eu" => format!("aiplatform.{region}.rep.googleapis.com"),
                _ => format!("{region}-aiplatform.googleapis.com"),
            };
            assert!(
                covered(&host),
                "{region} resolves to {host}, which is unbound"
            );
        }
    }
}

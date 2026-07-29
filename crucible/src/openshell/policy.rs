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
//! appended extra. `subtract` is exact-string, not glob-aware: the defaults ship both
//! `github.com:443:full` and `*.github.com:443:full` (the latter covers `api.github.com`), so
//! sealing GitHub requires denying both entries, denying only the apex leaves the wildcard,
//! and `api.github.com`, reachable.

use crate::manifest::OpenshellCfg;

/// Built-in egress endpoints in openshell's `host:port:access[:proto[:enforcement]]` form.
/// No protocol is given, so these are L4-only (CONNECT tunneling): `protocol=rest` would
/// enable L7 inspection, which blocks the CONNECT that Vertex streaming/gRPC clients use.
pub const DEFAULT_ENDPOINTS: &[&str] = &[
    "github.com:443:full",
    "*.github.com:443:full",
    "gitlab.com:443:full",
    "*.gitlab.com:443:full",
    "pypi.org:443:read-only",
    "files.pythonhosted.org:443:read-only",
    "aiplatform.googleapis.com:443:read-write",
    "*.aiplatform.googleapis.com:443:read-write",
    "oauth2.googleapis.com:443:read-write",
    "api.anthropic.com:443:read-write",
];

/// Built-in endpoints plus the domain's extras, de-duplicated, order preserved, then with
/// `deny_endpoints` subtracted. With `inherit_defaults = false` the built-ins are dropped and
/// only the domain's are allowed.
///
/// `broker_endpoint`, when `Some`, is the engine-resolved broker `host:port:access` entry. It is
/// appended **after** the deny subtraction and regardless of `inherit_defaults`, because the
/// broker is engine plumbing the domain opted into by enabling `[agent.broker]`, not a built-in
/// the domain can subtract. A broker-less domain (no broker_endpoint) with
/// `inherit_defaults = false` still resolves to exactly what its manifest lists.
pub fn resolve_endpoints(cfg: &OpenshellCfg, broker_endpoint: Option<&str>) -> Vec<String> {
    let merged = merge(inherited(cfg, DEFAULT_ENDPOINTS), &cfg.endpoints);
    let mut out = subtract(merged, &cfg.deny_endpoints);
    if let Some(ep) = broker_endpoint
        && !out.iter().any(|seen| seen == ep)
    {
        out.push(ep.to_string());
    }
    out
}

/// The harness's agent-CLI binaries (`defaults`, see [`crate::harness::Harness::default_binaries`])
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

/// Drop any entry in `list` that exact-string-matches one in `deny`. No glob semantics: denying
/// `"github.com:443:full"` does not touch `"*.github.com:443:full"`. A deny of an entry that
/// isn't present is a no-op.
fn subtract(list: Vec<String>, deny: &[String]) -> Vec<String> {
    list.into_iter()
        .filter(|item| !deny.iter().any(|d| d == item))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claude harness's binary defaults, what every pre-harness-boundary test resolved
    /// against.
    const DEFAULT_BINARIES: &[&str] = crate::harness::claude::DEFAULT_BINARIES;

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
        assert_eq!(resolve_endpoints(&c, None).len(), DEFAULT_ENDPOINTS.len());
        assert_eq!(resolve_binaries(&c, DEFAULT_BINARIES), DEFAULT_BINARIES);
    }

    #[test]
    fn extras_append_after_defaults() {
        let c = cfg(
            &["api.internal.example:443:read-write"],
            &["/usr/local/bin/kubectl"],
        );
        let eps = resolve_endpoints(&c, None);
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
            resolve_endpoints(&c, None),
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
            resolve_endpoints(&appended, None)
                .iter()
                .any(|e| e.contains("github.com"))
        );

        let c = sealed(
            &["registry.internal:443:read-only"],
            &["/usr/local/bin/claude"],
        );
        assert!(
            !resolve_endpoints(&c, None)
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
        assert!(resolve_endpoints(&c, None).is_empty());
        assert!(resolve_binaries(&c, DEFAULT_BINARIES).is_empty());
    }

    #[test]
    fn an_absent_openshell_table_still_inherits() {
        // `#[serde(default)]` on the field must not hand us `inherit_defaults = false`.
        let c = OpenshellCfg::default();
        assert!(c.inherit_defaults);
        assert_eq!(resolve_endpoints(&c, None).len(), DEFAULT_ENDPOINTS.len());
    }

    #[test]
    fn duplicate_extras_are_dropped() {
        // An extra that repeats a default must not appear twice.
        let c = cfg(&["github.com:443:full", "github.com:443:full"], &[]);
        let eps = resolve_endpoints(&c, None);
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
        let eps = resolve_endpoints(&c, Some(ep));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() + 1);
        assert_eq!(eps.last().unwrap(), ep);
    }

    #[test]
    fn broker_endpoint_appended_after_opt_out() {
        // A domain that opts out of defaults but enables the broker still gets the broker
        // endpoint: the broker is engine plumbing, not a domain-subtractable built-in.
        let c = sealed(&[], &[]);
        let ep = "host.containers.internal:8849:full";
        let eps = resolve_endpoints(&c, Some(ep));
        assert_eq!(eps, vec![ep]);
    }

    #[test]
    fn broker_endpoint_deduped_when_already_listed() {
        // If a (legacy) manifest still hand-lists the broker endpoint AND the engine passes
        // it, it must not appear twice.
        let ep = "host.containers.internal:8849:full";
        let c = cfg(&[ep], &[]);
        let eps = resolve_endpoints(&c, Some(ep));
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
        let eps = resolve_endpoints(&c, Some(ep));
        assert!(eps.contains(&ep.to_string()));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() + 1);
    }

    #[test]
    fn no_broker_no_append() {
        // A broker-less domain: None broker_endpoint, nothing extra appended.
        let c = OpenshellCfg::default();
        assert_eq!(
            resolve_endpoints(&c, None).len(),
            DEFAULT_ENDPOINTS.len(),
            "no broker means no extra endpoint"
        );
    }

    // --- deny_endpoints / deny_binaries (#192) ---

    #[test]
    fn deny_beats_an_inherited_default() {
        // Denying the apex only removes that one entry; the wildcard sibling is a separate
        // default and survives (see `deny_is_exact_string_not_glob` and
        // `deny_both_github_entries_seals_the_wildcard_too` for the actual GitHub seal).
        let c = OpenshellCfg {
            deny_endpoints: vec!["github.com:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, None);
        assert!(!eps.contains(&"github.com:443:full".to_string()));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() - 1);
    }

    #[test]
    fn deny_both_github_entries_seals_the_wildcard_too() {
        // The contamination guard, done correctly: sealing GitHub without opting out of the
        // rest of the built-in allowlist requires denying BOTH the apex and the wildcard, since
        // `*.github.com:443:full` (a separate default) covers `api.github.com`, the host an
        // in-loop agent would otherwise use to read the upstream fix off the REST API.
        let c = OpenshellCfg {
            deny_endpoints: vec![
                "github.com:443:full".to_string(),
                "*.github.com:443:full".to_string(),
            ],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, None);
        assert!(!eps.iter().any(|e| e.contains("github.com")));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len() - 2);
    }

    #[test]
    fn deny_beats_an_appended_extra() {
        let c = OpenshellCfg {
            endpoints: vec!["registry.internal:443:read-only".to_string()],
            deny_endpoints: vec!["registry.internal:443:read-only".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, None);
        assert!(!eps.contains(&"registry.internal:443:read-only".to_string()));
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len());
    }

    #[test]
    fn deny_of_an_absent_entry_is_a_no_op() {
        let c = OpenshellCfg {
            deny_endpoints: vec!["nowhere.example:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        assert_eq!(resolve_endpoints(&c, None).len(), DEFAULT_ENDPOINTS.len());

        let cb = OpenshellCfg {
            deny_binaries: vec!["/usr/local/bin/nope".to_string()],
            ..OpenshellCfg::default()
        };
        assert_eq!(resolve_binaries(&cb, DEFAULT_BINARIES), DEFAULT_BINARIES);
    }

    #[test]
    fn deny_is_exact_string_not_glob() {
        // Denying the apex entry must not touch the wildcard sibling, and vice versa.
        let c = OpenshellCfg {
            deny_endpoints: vec!["github.com:443:full".to_string()],
            ..OpenshellCfg::default()
        };
        let eps = resolve_endpoints(&c, None);
        assert!(!eps.contains(&"github.com:443:full".to_string()));
        assert!(eps.contains(&"*.github.com:443:full".to_string()));
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
        let eps = resolve_endpoints(&c, Some(ep));
        assert!(!eps.contains(&"github.com:443:full".to_string()));
        assert_eq!(eps.last().unwrap(), ep);
        assert_eq!(eps.len(), DEFAULT_ENDPOINTS.len());
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
}

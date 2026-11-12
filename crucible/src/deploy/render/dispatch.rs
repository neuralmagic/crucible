//! What the controller's own identity on a cluster may do in the loop namespace it dispatches run
//! pods into. The run render grants it to `[cluster] sandbox_readers`, so a cluster rendered from
//! a profile carries the grant the controller's dispatcher needs, and a test on that dispatcher
//! holds the two together.
//!
//! ```text
//!   pods              create get list watch delete   dispatch, the watch, collection
//!   pods/log          get                            the log relay
//!   pods/finalizers   update                         owner references to the pod (OpenShift)
//!   configmaps        create delete                  the pack ConfigMap
//!   secrets           create delete list patch       the run Secret; patch re-mints its token
//! ```
//!
//! No `get` on Secrets: the controller writes a run's Secret and never reads one back. `patch`
//! covers every Secret in the namespace, because a run's is named after its pod and a Role lists
//! exact names only.

use k8s_openapi::api::rbac::v1 as rbac;

/// The Role and RoleBinding the grant is rendered as, in the loop namespace.
pub const ROLE: &str = "crucible-dispatch";

fn rule(resource: &str, verbs: &[&str]) -> rbac::PolicyRule {
    rbac::PolicyRule {
        api_groups: Some(vec![String::new()]),
        resources: Some(vec![resource.to_string()]),
        verbs: verbs.iter().map(|v| v.to_string()).collect(),
        ..Default::default()
    }
}

/// The rules of [`ROLE`].
pub fn rules() -> Vec<rbac::PolicyRule> {
    vec![
        rule("pods", &["create", "get", "list", "watch", "delete"]),
        rule("pods/log", &["get"]),
        rule("pods/finalizers", &["update"]),
        rule("configmaps", &["create", "delete"]),
        rule("secrets", &["create", "delete", "list", "patch"]),
    ]
}

/// Whether `rules` let their subject make `verb` on `resource` of the core API group, on any name.
pub fn grants(rules: &[rbac::PolicyRule], resource: &str, verb: &str) -> bool {
    rules.iter().any(|rule| {
        rule.resource_names.is_none()
            && rule.api_groups.iter().flatten().any(String::is_empty)
            && rule.resources.iter().flatten().any(|r| r == resource)
            && rule.verbs.iter().any(|v| v == verb)
    })
}

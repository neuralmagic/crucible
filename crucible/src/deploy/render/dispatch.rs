//! The controller identity's grant in the loop namespace it dispatches run pods into.

use k8s_openapi::api::rbac::v1 as rbac;

/// The Role and RoleBinding the grant is rendered as.
pub const ROLE: &str = "crucible-dispatch";

fn rule(resource: &str, verbs: &[&str]) -> rbac::PolicyRule {
    rbac::PolicyRule {
        api_groups: Some(vec![String::new()]),
        resources: Some(vec![resource.to_string()]),
        verbs: verbs.iter().map(|v| v.to_string()).collect(),
        ..Default::default()
    }
}

pub fn rules() -> Vec<rbac::PolicyRule> {
    vec![
        rule("pods", &["create", "get", "list", "watch", "delete"]),
        rule("pods/log", &["get"]),
        rule("pods/finalizers", &["update"]),
        rule("configmaps", &["create", "delete"]),
        rule("secrets", &["create", "delete", "list", "patch"]),
    ]
}

/// Whether `rules` grant `verb` on core-group `resource`, unrestricted by name.
pub fn grants(rules: &[rbac::PolicyRule], resource: &str, verb: &str) -> bool {
    rules.iter().any(|rule| {
        rule.resource_names.is_none()
            && rule.api_groups.iter().flatten().any(String::is_empty)
            && rule.resources.iter().flatten().any(|r| r == resource)
            && rule.verbs.iter().any(|v| v == verb)
    })
}

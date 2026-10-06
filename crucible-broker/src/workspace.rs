//! The OpenShell workspace every gateway request from the engine and the broker acts in. The
//! gateway rejects a workspace-scoped request that carries no selector, so each request is built
//! through [`scoped`].

use openshell_core::proto::{
    ConfigureProviderRefreshRequest, CreateProviderRequest, CreateSandboxRequest,
    DeleteSandboxRequest, ExecSandboxRequest, GetProviderProfileRequest, GetProviderRequest,
    GetSandboxLogsRequest, GetSandboxPolicyStatusRequest, GetSandboxRequest,
    ImportProviderProfilesRequest, RotateProviderCredentialRequest, UpdateConfigRequest,
    UpdateProviderProfilesRequest, UpdateProviderRequest, WorkspaceSelector, workspace_selector,
};

/// The gateway's default workspace: crucible's gateway is single-tenant, one per loop pod.
pub const WORKSPACE: &str = "default";

/// A public gateway request that names the workspace it acts in.
pub trait WorkspaceScoped {
    fn workspace_scope_mut(&mut self) -> &mut Option<WorkspaceSelector>;
}

macro_rules! workspace_scoped {
    ($($request:ty),+ $(,)?) => {
        $(impl WorkspaceScoped for $request {
            fn workspace_scope_mut(&mut self) -> &mut Option<WorkspaceSelector> {
                &mut self.workspace_scope
            }
        })+
    };
}

workspace_scoped!(
    ConfigureProviderRefreshRequest,
    CreateProviderRequest,
    CreateSandboxRequest,
    DeleteSandboxRequest,
    ExecSandboxRequest,
    GetProviderProfileRequest,
    GetProviderRequest,
    GetSandboxLogsRequest,
    GetSandboxPolicyStatusRequest,
    GetSandboxRequest,
    ImportProviderProfilesRequest,
    RotateProviderCredentialRequest,
    UpdateConfigRequest,
    UpdateProviderProfilesRequest,
    UpdateProviderRequest,
);

/// `request` addressed to [`WORKSPACE`].
pub fn scoped<R: WorkspaceScoped>(mut request: R) -> R {
    *request.workspace_scope_mut() = Some(workspace_selector(WORKSPACE));
    request
}

/// The request struct literals in `source` (outside its trailing `mod tests`) that are not built
/// inside a [`scoped`] call. `HealthRequest` is exempt: health is not workspace-scoped. Lets each
/// crate's tests prove its gateway client cannot send an unscoped request.
pub fn unscoped_request_literals(source: &str) -> Vec<String> {
    let body = source
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .unwrap_or(source);
    let mut unscoped = Vec::new();
    for (at, _) in body.match_indices("Request {") {
        let name_start = body[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let name = &body[name_start..at + "Request".len()];
        if !name.starts_with(|c: char| c.is_ascii_uppercase()) || name == "HealthRequest" {
            continue;
        }
        if !body[..name_start].trim_end().ends_with("scoped(") {
            unscoped.push(name.to_string());
        }
    }
    unscoped
}

#[cfg(test)]
mod tests {
    use crate::workspace::{WORKSPACE, scoped, unscoped_request_literals};
    use openshell_core::proto::workspace_selector::Selection;
    use openshell_core::proto::{GetSandboxRequest, UpdateConfigRequest};

    #[test]
    fn a_scoped_request_names_the_default_workspace() {
        let request = scoped(GetSandboxRequest {
            name: "s".into(),
            ..Default::default()
        });
        assert_eq!(
            request.workspace_scope.and_then(|s| s.selection),
            Some(Selection::Workspace(WORKSPACE.to_string()))
        );
        assert_eq!(WORKSPACE, "default");
    }

    #[test]
    fn scoping_keeps_every_other_field() {
        let request = scoped(UpdateConfigRequest {
            sandbox: "s".into(),
            setting_key: "k".into(),
            ..Default::default()
        });
        assert_eq!(request.sandbox, "s");
        assert_eq!(request.setting_key, "k");
    }

    #[test]
    fn a_request_literal_outside_scoped_is_reported() {
        let source = "fn f() {\n    client.exec_sandbox(ExecSandboxRequest {\n        x\n    });\n    \
                      client.get_sandbox(scoped(GetSandboxRequest {\n        y\n    }));\n    \
                      client.health(HealthRequest {});\n}\n\
                      #[cfg(test)]\nmod tests {\n    DeleteSandboxRequest { z };\n}\n";
        assert_eq!(
            unscoped_request_literals(source),
            vec!["ExecSandboxRequest"]
        );
    }

    #[test]
    fn a_let_bound_scoped_request_passes() {
        let source = "let request = scoped(\n    ExecSandboxRequest {\n        x\n    },\n);";
        assert!(unscoped_request_literals(source).is_empty());
    }

    #[test]
    fn every_broker_gateway_request_is_scoped() {
        assert_eq!(
            unscoped_request_literals(include_str!("gateway.rs")),
            Vec::<String>::new()
        );
    }
}

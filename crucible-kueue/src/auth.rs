//! Request authentication: an OAuth access token from the crucible realm, or a registered tenant
//! token. Either way the request leaves with a [`Caller`] (what the tools enforce) and a
//! [`Principal`] (what the tool policy's rules see) in its extensions.
//!
//! The two are told apart by shape: an access token is a compact JWS (three dot-separated
//! parts), a tenant token is opaque hex. A JWT never falls back to the tenant registry.

use crate::tenant::{Caller, Launcher, NameError, Namespace, QueueName, Registry, RunId};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use crucible_contract::kueue::TenantRole;
use crucible_mcp_auth::principal::Principal;
use crucible_mcp_auth::server::{AuthError, ResourceServer, bearer_token};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Scope that lets a token read queues and workloads.
pub const SCOPE_READ: &str = "kueue.read";
/// Scope that lets a token submit and control workloads.
pub const SCOPE_SUBMIT: &str = "kueue.submit";

pub struct Auth {
    pub registry: Arc<Registry>,
    /// Accept access tokens from this authorization server. `None` accepts tenant tokens only.
    pub oidc: Option<Arc<ResourceServer>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("token claim {0} is missing or not a string")]
    Missing(&'static str),
    #[error("token claim queues must be a list of strings")]
    Queues,
    #[error("token claim role {0:?} is neither viewer nor submitter")]
    Role(String),
    #[error(transparent)]
    Name(#[from] NameError),
}

impl Caller {
    /// The tenant scope a realm token carries in its `namespace`, `role`, and `queues` claims,
    /// acting for the token's subject. A run's token names its run in `run_id`; a person's token
    /// has none and owns its workloads under its own subject.
    pub fn from_principal(principal: &Principal) -> Result<Self, ClaimError> {
        let claim = |key: &'static str| {
            principal
                .claims
                .get(key)
                .and_then(Value::as_str)
                .ok_or(ClaimError::Missing(key))
        };
        let role = claim("role")?;
        let role = serde_json::from_value::<TenantRole>(json!(role))
            .map_err(|_| ClaimError::Role(role.to_string()))?;
        let queues = match principal.claims.get("queues") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|q| {
                    q.as_str()
                        .ok_or(ClaimError::Queues)
                        .and_then(|q| Ok(QueueName::parse(q)?))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(ClaimError::Queues),
        };
        Ok(Self {
            run_id: match principal.claims.get("run_id") {
                None => RunId::parse(&principal.subject)?,
                Some(_) => RunId::parse(claim("run_id")?)?,
            },
            namespace: Namespace::parse(claim("namespace")?)?,
            role,
            queues,
            launched_by: Some(Launcher::parse(&principal.subject)?),
        })
    }

    /// The principal a tenant token stands for, with claims shaped like a realm token's so one
    /// tool policy covers both. Its scopes follow the tenant role.
    pub fn tenant_principal(&self) -> Principal {
        let scope = match self.role {
            TenantRole::Viewer => SCOPE_READ.to_string(),
            TenantRole::Submitter => format!("{SCOPE_READ} {SCOPE_SUBMIT}"),
        };
        let subject = format!("tenant:{}", self.run_id);
        let scopes = scope.split(' ').map(str::to_string).collect();
        let mut claims = Map::new();
        claims.insert("sub".into(), json!(subject));
        claims.insert("run_id".into(), json!(self.run_id.as_str()));
        claims.insert("namespace".into(), json!(self.namespace.as_str()));
        claims.insert("role".into(), json!(self.role));
        claims.insert(
            "queues".into(),
            json!(
                self.queues
                    .iter()
                    .map(QueueName::as_str)
                    .collect::<Vec<_>>()
            ),
        );
        claims.insert("scope".into(), json!(scope));
        Principal {
            subject,
            actor_chain: Vec::new(),
            client_id: None,
            namespace: Some(self.namespace.to_string()),
            scopes,
            claims,
        }
    }
}

fn is_jwt(token: &str) -> bool {
    token.split('.').count() == 3
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn refused(status: StatusCode, error: &str, description: String) -> Response {
    (
        status,
        axum::Json(json!({"status": "error", "error": error, "error_description": description})),
    )
        .into_response()
}

/// Middleware: authenticate the bearer token and attach [`Caller`] and [`Principal`].
pub async fn authenticate(State(auth): State<Arc<Auth>>, mut req: Request, next: Next) -> Response {
    let headers = req.headers();
    let Some(token) = bearer_token(headers) else {
        return match &auth.oidc {
            Some(oidc) => oidc.challenge(headers, &AuthError::Missing),
            None => refused(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing bearer token".into(),
            ),
        };
    };
    let (caller, principal, via) = match (&auth.oidc, is_jwt(token)) {
        (Some(oidc), true) => {
            let principal = match oidc.authenticate(headers).await {
                Ok(p) => p,
                Err(error) => {
                    tracing::info!(auth = "oidc", %error, "token refused");
                    return oidc.challenge(headers, &error);
                }
            };
            match Caller::from_principal(&principal) {
                Ok(caller) => (caller, principal, "oidc"),
                Err(error) => {
                    tracing::info!(auth = "oidc", sub = %principal.subject, %error, "token carries no tenant scope");
                    return refused(
                        StatusCode::FORBIDDEN,
                        "insufficient_scope",
                        error.to_string(),
                    );
                }
            }
        }
        _ => match auth.registry.authenticate(token, unix_now()).await {
            Some(caller) => {
                let principal = caller.tenant_principal();
                (caller, principal, "tenant-token")
            }
            None => {
                tracing::info!(auth = "tenant-token", "unknown bearer token");
                return match &auth.oidc {
                    Some(oidc) => oidc.challenge(headers, &AuthError::Unrecognized),
                    None => refused(
                        StatusCode::UNAUTHORIZED,
                        "unauthorized",
                        "missing or unknown bearer token".into(),
                    ),
                };
            }
        },
    };
    tracing::info!(
        auth = via,
        sub = %principal.subject,
        presenter = principal.presenter(),
        run = %caller.run_id,
        namespace = %caller.namespace,
        "caller authenticated"
    );
    req.extensions_mut().insert(caller);
    req.extensions_mut().insert(principal);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use crate::auth::*;
    use crate::tenant::Tenants;
    use axum::body::{Body, to_bytes};
    use crucible_contract::kueue::token_digest;
    use tower::ServiceExt;

    fn principal(claims: Value) -> Principal {
        match claims {
            Value::Object(m) => Principal::from_claims(m).unwrap(),
            other => panic!("{other}"),
        }
    }

    fn realm_claims() -> Value {
        json!({
            "sub": "weaton",
            "act": {"sub": "spiffe://pirate.crucible/openshell/sandbox/s1"},
            "run_id": "auth-spike",
            "namespace": "crucible-kueue-e2e",
            "role": "submitter",
            "queues": ["crucible-e2e"],
            "scope": "kueue.read kueue.submit",
        })
    }

    #[test]
    fn a_realm_token_maps_to_a_caller_acting_for_its_subject() {
        let c = Caller::from_principal(&principal(realm_claims())).unwrap();
        assert_eq!(c.run_id.as_str(), "auth-spike");
        assert_eq!(c.namespace.as_str(), "crucible-kueue-e2e");
        assert_eq!(c.role, TenantRole::Submitter);
        assert_eq!(c.queues, [QueueName::parse("crucible-e2e").unwrap()]);
        assert_eq!(c.launched_by, Some(Launcher::parse("weaton").unwrap()));
    }

    #[test]
    fn a_persons_token_owns_its_workloads_under_its_subject() {
        let mut claims = realm_claims();
        let map = claims.as_object_mut().unwrap();
        map.remove("run_id");
        map.remove("act");
        map.insert("sub".into(), json!("3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90"));
        let c = Caller::from_principal(&principal(claims)).unwrap();
        assert_eq!(c.run_id.as_str(), "3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90");
        assert_eq!(
            c.launched_by.unwrap().as_str(),
            "3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90"
        );
    }

    #[test]
    fn absent_queues_mean_every_queue() {
        let mut claims = realm_claims();
        claims.as_object_mut().unwrap().remove("queues");
        assert!(
            Caller::from_principal(&principal(claims))
                .unwrap()
                .queues
                .is_empty()
        );
    }

    #[test]
    fn a_token_without_valid_tenant_claims_is_refused() {
        let cases = [
            ("run_id", Some(json!(7))),
            ("namespace", None),
            ("role", None),
            ("role", Some(json!("admin"))),
            ("namespace", Some(json!("Not_A_Label"))),
            ("queues", Some(json!("crucible-e2e"))),
            ("queues", Some(json!([1]))),
            ("sub", Some(json!("weaton@example.com"))),
        ];
        for (key, value) in cases {
            let mut claims = realm_claims();
            let map = claims.as_object_mut().unwrap();
            match &value {
                Some(v) => map.insert(key.into(), v.clone()),
                None => map.remove(key),
            };
            assert!(
                Caller::from_principal(&principal(claims)).is_err(),
                "{key} = {value:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_tenant_token_principal_carries_the_same_claim_shape() {
        let tenants = Tenants::from_toml(&format!(
            "[[tenant]]\nrun_id = \"run-1\"\ntoken_sha256 = \"{}\"\nnamespace = \"team-a\"\nrole = \"viewer\"\nqueues = [\"gpu\"]\n",
            token_digest("t")
        ))
        .unwrap();
        let registry = Registry::new(tenants);
        let caller = registry.authenticate("t", 0).await.unwrap();
        let p = caller.tenant_principal();
        assert_eq!(p.subject, "tenant:run-1");
        assert_eq!(p.namespace.as_deref(), Some("team-a"));
        assert_eq!(
            p.scopes.iter().map(String::as_str).collect::<Vec<_>>(),
            ["kueue.read"]
        );
        assert_eq!(
            Value::Object(p.claims.clone()),
            json!({"sub": "tenant:run-1", "run_id": "run-1", "namespace": "team-a", "role": "viewer",
                   "queues": ["gpu"], "scope": "kueue.read"})
        );
        assert_eq!(caller.launched_by, None);
    }

    fn app(registry: Registry) -> axum::Router {
        let auth = Auth {
            registry: Arc::new(registry),
            oidc: None,
        };
        axum::Router::new()
            .route(
                "/mcp",
                axum::routing::post(|req: Request| async move {
                    let caller = req.extensions().get::<Caller>().unwrap();
                    let principal = req.extensions().get::<Principal>().unwrap();
                    format!("{} {}", caller.run_id, principal.subject)
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(auth),
                authenticate,
            ))
    }

    async fn post(app: axum::Router, authorization: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::builder().method("POST").uri("/mcp");
        if let Some(a) = authorization {
            req = req.header(axum::http::header::AUTHORIZATION, a);
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn tenant_tokens_authenticate_without_oidc() {
        let tenant = |token: &str| {
            Tenants::from_toml(&format!(
                "[[tenant]]\nrun_id = \"run-1\"\ntoken_sha256 = \"{}\"\nnamespace = \"team-a\"\nrole = \"submitter\"\n",
                token_digest(token)
            ))
            .unwrap()
        };
        assert_eq!(
            post(app(Registry::new(tenant("opaque"))), Some("Bearer opaque")).await,
            (StatusCode::OK, "run-1 tenant:run-1".into())
        );
        let (status, _) = post(app(Registry::new(tenant("a.b.c"))), Some("Bearer a.b.c")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "without OIDC a JWT-shaped tenant token still authenticates"
        );
    }

    #[tokio::test]
    async fn missing_and_unknown_tokens_are_unauthorized() {
        let app = app(Registry::new(Tenants::default()));
        assert_eq!(post(app.clone(), None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            post(app.clone(), Some("Bearer nope")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post(app, Some("Basic nope")).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}

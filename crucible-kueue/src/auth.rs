//! Request authentication: an OAuth access token from the crucible realm, or a registered tenant
//! token. Either way the request leaves with an [`Identity`] (what the tools resolve into a
//! [`Caller`] per call) and a [`Principal`] (what the tool policy's rules see) in its extensions.
//!
//! | Token                    | Identity                                                        |
//! |--------------------------|-----------------------------------------------------------------|
//! | tenant token             | pinned to its record's namespace, role, queues                  |
//! | realm token with run_id  | a run: pinned to its `namespace` claim, `role`, `queues`        |
//! | realm token, no run_id   | a person: any namespace, decided per call by the policy's access |
//! |                          | checks; accepted only when the server has an access reviewer    |
//!
//! The two are told apart by shape: an access token is a compact JWS (three dot-separated
//! parts), a tenant token is opaque hex. A JWT never falls back to the tenant registry.

use crate::tenant::{Caller, Identity, Launcher, NameError, Namespace, QueueName, Registry, RunId};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use crucible_contract::kueue::TenantRole;
use crucible_mcp_auth::principal::Principal;
use crucible_mcp_auth::server::{AuthError, ResourceServer, bearer_token};
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub struct Auth {
    pub registry: Arc<Registry>,
    /// Accept access tokens from this authorization server. `None` accepts tenant tokens only.
    pub oidc: Option<Arc<ResourceServer>>,
    /// Accept people's tokens (no `run_id`). Only safe when the tool policy's access checks
    /// decide every call, so the server sets it exactly when it has an access reviewer.
    pub people: bool,
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
    #[error("this server accepts only run tokens: it has no access reviewer for people")]
    People,
    #[error("a person's token needs a kube_user claim")]
    NoKubeUser,
}

impl Identity {
    /// The identity a realm token carries. A run's token (`run_id`) is pinned to its `namespace`
    /// claim with its `role` and `queues`. A person's token (no `run_id`) needs `people` and a
    /// `kube_user` claim; it owns its workloads under its own subject, may name any namespace
    /// (its `namespace` claim, if any, is the default), and is limited by the access checks.
    pub fn from_principal(principal: &Principal, people: bool) -> Result<Self, ClaimError> {
        let claim = |key: &'static str| {
            principal
                .claims
                .get(key)
                .and_then(Value::as_str)
                .ok_or(ClaimError::Missing(key))
        };
        let launched_by = Some(Launcher::parse(&principal.subject)?);
        if !principal.claims.contains_key("run_id") {
            if !people {
                return Err(ClaimError::People);
            }
            if !principal.claims.contains_key("kube_user") {
                return Err(ClaimError::NoKubeUser);
            }
            let namespace = match principal.claims.get("namespace") {
                None | Some(Value::Null) => None,
                Some(_) => Some(Namespace::parse(claim("namespace")?)?),
            };
            return Ok(Self {
                run_id: RunId::parse(&principal.subject)?,
                namespace,
                pinned: false,
                role: TenantRole::Submitter,
                queues: Vec::new(),
                launched_by,
            });
        }
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
            run_id: RunId::parse(claim("run_id")?)?,
            namespace: Some(Namespace::parse(claim("namespace")?)?),
            pinned: true,
            role,
            queues,
            launched_by,
        })
    }
}

impl Caller {
    /// The principal a tenant token stands for, with claims shaped like a realm token's so one
    /// tool policy covers both. Its scopes follow the tenant role.
    pub fn tenant_principal(&self) -> Principal {
        let scope = self.role.scopes().join(" ");
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
    let (identity, principal, via) = match (&auth.oidc, is_jwt(token)) {
        (Some(oidc), true) => {
            let principal = match oidc.authenticate(headers).await {
                Ok(p) => p,
                Err(error) => {
                    tracing::info!(auth = "oidc", %error, "token refused");
                    return oidc.challenge(headers, &error);
                }
            };
            match Identity::from_principal(&principal, auth.people) {
                Ok(identity) => (identity, principal, "oidc"),
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
                (Identity::pinned(caller), principal, "tenant-token")
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
        run = %identity.run_id,
        namespace = identity.namespace.as_ref().map(Namespace::as_str),
        pinned = identity.pinned,
        "caller authenticated"
    );
    req.extensions_mut().insert(identity);
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
            "sub": "alice",
            "act": {"sub": "spiffe://example.crucible/openshell/sandbox/s1"},
            "run_id": "auth-spike",
            "namespace": "team-e2e",
            "role": "submitter",
            "queues": ["crucible-e2e"],
            "scope": "kueue.read kueue.submit",
        })
    }

    fn run_caller(claims: Value) -> Result<Caller, ClaimError> {
        let identity = Identity::from_principal(&principal(claims), false)?;
        assert!(identity.pinned);
        Ok(identity.in_namespace(None).unwrap())
    }

    #[test]
    fn a_run_token_is_pinned_and_acts_for_its_subject() {
        let c = run_caller(realm_claims()).unwrap();
        assert_eq!(c.run_id.as_str(), "auth-spike");
        assert_eq!(c.namespace.as_str(), "team-e2e");
        assert_eq!(c.role, TenantRole::Submitter);
        assert_eq!(c.queues, [QueueName::parse("crucible-e2e").unwrap()]);
        assert_eq!(c.launched_by, Some(Launcher::parse("alice").unwrap()));
    }

    fn person_claims() -> Value {
        json!({
            "sub": "3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90",
            "kube_user": "alice",
            "scope": "kueue.read kueue.submit",
        })
    }

    #[test]
    fn a_persons_token_picks_its_namespace_and_owns_its_workloads_under_its_subject() {
        let identity = Identity::from_principal(&principal(person_claims()), true).unwrap();
        assert!(!identity.pinned);
        assert_eq!(identity.role, TenantRole::Submitter);
        assert!(identity.queues.is_empty());
        assert!(matches!(
            identity.in_namespace(None),
            Err(crate::tenant::Refusal::NamespaceRequired)
        ));
        let c = identity.in_namespace(Some("team-b")).unwrap();
        assert_eq!(c.namespace.as_str(), "team-b");
        assert_eq!(c.run_id.as_str(), "3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90");
        assert_eq!(
            c.launched_by.unwrap().as_str(),
            "3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90"
        );

        let mut with_default = person_claims();
        with_default["namespace"] = json!("team-a");
        let identity = Identity::from_principal(&principal(with_default), true).unwrap();
        assert_eq!(
            identity.in_namespace(None).unwrap().namespace.as_str(),
            "team-a"
        );
        assert_eq!(
            identity
                .in_namespace(Some("team-b"))
                .unwrap()
                .namespace
                .as_str(),
            "team-b"
        );
    }

    #[test]
    fn people_need_a_reviewing_server_and_a_kube_user() {
        assert!(matches!(
            Identity::from_principal(&principal(person_claims()), false),
            Err(ClaimError::People)
        ));
        let mut no_user = person_claims();
        no_user.as_object_mut().unwrap().remove("kube_user");
        assert!(matches!(
            Identity::from_principal(&principal(no_user), true),
            Err(ClaimError::NoKubeUser)
        ));
    }

    #[test]
    fn absent_queues_mean_every_queue() {
        let mut claims = realm_claims();
        claims.as_object_mut().unwrap().remove("queues");
        assert!(run_caller(claims).unwrap().queues.is_empty());
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
            ("sub", Some(json!("alice@example.com"))),
        ];
        for (key, value) in cases {
            let mut claims = realm_claims();
            let map = claims.as_object_mut().unwrap();
            match &value {
                Some(v) => map.insert(key.into(), v.clone()),
                None => map.remove(key),
            };
            assert!(
                Identity::from_principal(&principal(claims), true).is_err(),
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
            people: false,
        };
        axum::Router::new()
            .route(
                "/mcp",
                axum::routing::post(|req: Request| async move {
                    let caller = req
                        .extensions()
                        .get::<Identity>()
                        .unwrap()
                        .in_namespace(None)
                        .unwrap();
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

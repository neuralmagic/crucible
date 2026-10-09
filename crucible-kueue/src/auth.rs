//! Request authentication: an OAuth access token from a trusted issuer. The request leaves with
//! an [`Identity`] (what the tools resolve into a [`crate::caller::Caller`] per call) and a [`Principal`] (what
//! the guard's rules see) in its extensions.
//!
//! | Token                    | Identity                                                        |
//! |--------------------------|-----------------------------------------------------------------|
//! | from a run issuer        | a run: pinned to its `namespace` claim, `role`, `queues`        |
//! | from a people issuer     | a person: any namespace, decided per call by the access checks; |
//! |                          | accepted only when the server has an access reviewer            |

use crate::caller::{Identity, Launcher, NameError, Namespace, QueueName, RunId};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use crucible_contract::kueue::TenantRole;
use crucible_mcp_auth::kube::USER_CLAIM;
use crucible_mcp_auth::principal::{Callers, Principal};
use crucible_mcp_auth::server::ResourceServer;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Auth {
    pub oidc: Arc<ResourceServer>,
    /// Accept people's tokens (from a people issuer). Only safe when the tool policy's access
    /// checks decide every call, so the server sets it exactly when it has an access reviewer.
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
    #[error("a run's token must not come from a people issuer")]
    RunFromPeopleIssuer,
    #[error("a person's token needs a kube_user claim")]
    NoKubeUser,
}

impl Identity {
    /// The identity an access token carries; its issuer decides which kind. A run's token is
    /// pinned to its `namespace` claim with its `role` and `queues`, and its presenter (`act.sub`)
    /// is recorded as the actor. Without `role` it submits if it carries `kube_user`, its
    /// launcher's RBAC deciding what it may do; without `queues` it may use every queue. A person's token needs `people` and a `kube_user` claim; it owns
    /// its workloads under its own subject, may name any namespace (its `namespace` claim, if any,
    /// is the default), and is limited by the access checks.
    pub fn from_principal(principal: &Principal, people: bool) -> Result<Self, ClaimError> {
        let claim = |key: &'static str| {
            principal
                .claims
                .get(key)
                .and_then(Value::as_str)
                .ok_or(ClaimError::Missing(key))
        };
        let launched_by = Some(Launcher::parse(&principal.subject)?);
        if principal.callers == Callers::People {
            if principal.claims.contains_key("run_id") {
                return Err(ClaimError::RunFromPeopleIssuer);
            }
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
                actor: None,
            });
        }
        let role = match principal.claims.get("role") {
            None | Some(Value::Null) if principal.claims.contains_key(USER_CLAIM) => {
                TenantRole::Submitter
            }
            _ => {
                let role = claim("role")?;
                serde_json::from_value::<TenantRole>(json!(role))
                    .map_err(|_| ClaimError::Role(role.to_string()))?
            }
        };
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
            actor: principal.actor_chain.first().cloned(),
        })
    }
}

fn refused(status: StatusCode, error: &str, description: String) -> Response {
    (
        status,
        axum::Json(json!({"status": "error", "error": error, "error_description": description})),
    )
        .into_response()
}

/// Middleware: authenticate the bearer token and attach [`Identity`] and [`Principal`].
pub async fn authenticate(State(auth): State<Arc<Auth>>, mut req: Request, next: Next) -> Response {
    let headers = req.headers();
    let principal = match auth.oidc.authenticate(headers).await {
        Ok(p) => p,
        Err(error) => {
            tracing::info!(%error, "token refused");
            return auth.oidc.challenge(headers, &error);
        }
    };
    let identity = match Identity::from_principal(&principal, auth.people) {
        Ok(identity) => identity,
        Err(error) => {
            tracing::info!(sub = %principal.subject, %error, "token identifies no caller this server serves");
            return refused(
                StatusCode::FORBIDDEN,
                "insufficient_scope",
                "the access token does not identify a run or a person this server serves".into(),
            );
        }
    };
    tracing::info!(
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
    use crate::caller::Caller;
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;

    fn principal(claims: Value) -> Principal {
        match claims {
            Value::Object(m) => Principal::from_claims(m, Callers::Runs).unwrap(),
            other => panic!("{other}"),
        }
    }

    fn person(claims: Value) -> Principal {
        match claims {
            Value::Object(m) => Principal::from_claims(m, Callers::People).unwrap(),
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
    fn the_issuer_not_the_claims_decides_person_or_run() {
        let mut no_run = realm_claims();
        no_run.as_object_mut().unwrap().remove("run_id");
        assert!(matches!(
            Identity::from_principal(&principal(no_run), true),
            Err(ClaimError::Missing("run_id"))
        ));
        let mut run_shaped = person_claims();
        run_shaped["run_id"] = json!("auth-spike");
        assert!(matches!(
            Identity::from_principal(&person(run_shaped), true),
            Err(ClaimError::RunFromPeopleIssuer)
        ));
    }

    #[test]
    fn a_runs_presenter_is_its_actor_and_a_person_has_none() {
        let identity = Identity::from_principal(&principal(realm_claims()), false).unwrap();
        assert_eq!(
            identity.actor.as_deref(),
            Some("spiffe://example.crucible/openshell/sandbox/s1")
        );
        let identity = Identity::from_principal(&person(person_claims()), true).unwrap();
        assert_eq!(identity.actor, None);
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
        let identity = Identity::from_principal(&person(person_claims()), true).unwrap();
        assert!(!identity.pinned);
        assert_eq!(identity.role, TenantRole::Submitter);
        assert!(identity.queues.is_empty());
        assert!(matches!(
            identity.in_namespace(None),
            Err(crate::caller::Refusal::NamespaceRequired)
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
        let identity = Identity::from_principal(&person(with_default), true).unwrap();
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
            Identity::from_principal(&person(person_claims()), false),
            Err(ClaimError::People)
        ));
        let mut no_user = person_claims();
        no_user.as_object_mut().unwrap().remove("kube_user");
        assert!(matches!(
            Identity::from_principal(&person(no_user), true),
            Err(ClaimError::NoKubeUser)
        ));
    }

    #[test]
    fn a_run_without_role_or_queues_submits_under_its_launchers_rbac() {
        let mut claims = realm_claims();
        let map = claims.as_object_mut().unwrap();
        map.remove("role");
        map.remove("queues");
        map.insert("kube_user".into(), json!("alice"));
        let c = run_caller(claims.clone()).unwrap();
        assert_eq!(c.role, TenantRole::Submitter);
        assert!(c.queues.is_empty());

        claims.as_object_mut().unwrap().remove("kube_user");
        assert!(matches!(
            Identity::from_principal(&principal(claims), false),
            Err(ClaimError::Missing("role"))
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

    async fn app() -> (crucible_mcp_auth::testing::JwksServer, axum::Router) {
        let jwks =
            crucible_mcp_auth::testing::JwksServer::start(vec![crucible_mcp_auth::testing::jwk(
                "a",
            )])
            .await;
        let oidc = crucible_mcp_auth::testing::resource_server(
            &jwks,
            std::time::Duration::from_secs(300),
            std::time::Duration::from_secs(1),
        );
        let auth = Auth {
            oidc: Arc::new(oidc),
            people: false,
        };
        let app = axum::Router::new()
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
            ));
        (jwks, app)
    }

    async fn post(app: axum::Router, authorization: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(axum::http::header::HOST, "kueue.apps.example");
        if let Some(a) = authorization {
            req = req.header(axum::http::header::AUTHORIZATION, a);
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_run_token_authenticates_and_anything_else_is_challenged() {
        let (_jwks, app) = app().await;
        let token =
            crucible_mcp_auth::testing::sign("a", &crucible_mcp_auth::testing::run_claims());
        assert_eq!(
            post(app.clone(), Some(&format!("Bearer {token}"))).await,
            (StatusCode::OK, "auth-spike alice".into())
        );
        for authorization in [None, Some("Bearer opaque-tenant-token"), Some("Basic nope")] {
            assert_eq!(
                post(app.clone(), authorization).await.0,
                StatusCode::UNAUTHORIZED,
                "{authorization:?}"
            );
        }
        let foreign =
            crucible_mcp_auth::testing::sign("b", &crucible_mcp_auth::testing::run_claims());
        assert_eq!(
            post(app, Some(&format!("Bearer {foreign}"))).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}

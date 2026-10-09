//! Request authentication: an OAuth access token from a trusted issuer. The request leaves with
//! an [`Identity`] (what the tools resolve into a [`crate::caller::Caller`] per call) and a
//! [`Principal`] (what the guard's rules see) in its extensions.
//!
//! A token stands for a run (from a run issuer, carrying `run_id`) or a person (from a people
//! issuer), and either way names the Kubernetes user it acts as (`kube_user`): the launcher for a
//! run, the person themselves otherwise. That user's RBAC decides every call; the token's
//! `namespace` claim is only a default.

use crate::caller::{Identity, Launcher, NameError, Namespace, RunId};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use crucible_mcp_auth::kube::USER_CLAIM;
use crucible_mcp_auth::principal::{Callers, Principal};
use crucible_mcp_auth::server::ResourceServer;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Auth {
    pub oidc: Arc<ResourceServer>,
    /// Accept people's tokens (from a people issuer). The server sets it exactly when it has an
    /// access reviewer, without which no person's call could be checked.
    pub people: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("token claim {0} is missing or not a string")]
    Missing(&'static str),
    #[error(transparent)]
    Name(#[from] NameError),
    #[error("this server accepts only run tokens: it has no access reviewer for people")]
    People,
    #[error("a run's token must not come from a people issuer")]
    RunFromPeopleIssuer,
    #[error("the token names no Kubernetes user (kube_user) to act as")]
    NoKubeUser,
}

impl Identity {
    /// The identity an access token carries; its issuer decides which kind. Both kinds need
    /// `kube_user`. A run's is its `run_id` and its presenter (`act.sub`) is recorded as the actor;
    /// a person's needs `people` and owns its workloads under its own subject. `role` and `queues`
    /// claims are ignored.
    pub fn from_principal(principal: &Principal, people: bool) -> Result<Self, ClaimError> {
        let claim = |key: &'static str| {
            principal
                .claims
                .get(key)
                .and_then(Value::as_str)
                .ok_or(ClaimError::Missing(key))
        };
        let is_person = principal.callers == Callers::People;
        if is_person && principal.claims.contains_key("run_id") {
            return Err(ClaimError::RunFromPeopleIssuer);
        }
        if is_person && !people {
            return Err(ClaimError::People);
        }
        if !principal.claims.contains_key(USER_CLAIM) {
            return Err(ClaimError::NoKubeUser);
        }
        let kube_user = claim(USER_CLAIM)?.to_string();
        let namespace = match principal.claims.get("namespace") {
            None | Some(Value::Null) => None,
            Some(_) => Some(Namespace::parse(claim("namespace")?)?),
        };
        let (run_id, actor) = match is_person {
            true => (RunId::parse(&principal.subject)?, None),
            false => (
                RunId::parse(claim("run_id")?)?,
                principal.actor_chain.first().cloned(),
            ),
        };
        Ok(Self {
            run_id,
            namespace,
            launched_by: Launcher::parse(&principal.subject)?,
            kube_user,
            actor,
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
            "kube_user": "alice",
            "namespace": "team-e2e",
            "scope": "kueue.read kueue.submit",
        })
    }

    fn person_claims() -> Value {
        json!({
            "sub": "3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90",
            "kube_user": "alice",
            "scope": "kueue.read kueue.submit",
        })
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
    fn a_run_acts_for_its_launcher_with_its_namespace_as_the_default() {
        let identity = Identity::from_principal(&principal(realm_claims()), false).unwrap();
        assert_eq!(
            identity.actor.as_deref(),
            Some("spiffe://example.crucible/openshell/sandbox/s1")
        );
        let c: Caller = identity.in_namespace(None).unwrap();
        assert_eq!(c.run_id.as_str(), "auth-spike");
        assert_eq!(c.namespace.as_str(), "team-e2e");
        assert_eq!(c.launched_by, Launcher::parse("alice").unwrap());
        assert_eq!(c.kube_user, "alice");
        assert_eq!(
            identity
                .in_namespace(Some("team-b"))
                .unwrap()
                .namespace
                .as_str(),
            "team-b",
            "the launcher's RBAC, not the token, decides whether it may act there"
        );
    }

    #[test]
    fn a_persons_token_picks_its_namespace_and_owns_its_workloads_under_its_subject() {
        let identity = Identity::from_principal(&person(person_claims()), true).unwrap();
        assert_eq!(identity.actor, None);
        assert!(matches!(
            identity.in_namespace(None),
            Err(crate::caller::Refusal::NamespaceRequired)
        ));
        let c = identity.in_namespace(Some("team-b")).unwrap();
        assert_eq!(c.namespace.as_str(), "team-b");
        assert_eq!(c.run_id.as_str(), "3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90");
        assert_eq!(
            c.launched_by.as_str(),
            "3f1c9a2e-7b4d-4e8a-9c51-0d2e6f7a8b90"
        );

        let mut with_default = person_claims();
        with_default["namespace"] = json!("team-a");
        let identity = Identity::from_principal(&person(with_default), true).unwrap();
        assert_eq!(
            identity.in_namespace(None).unwrap().namespace.as_str(),
            "team-a"
        );
    }

    #[test]
    fn every_token_needs_a_kubernetes_user_and_people_a_reviewing_server() {
        assert!(matches!(
            Identity::from_principal(&person(person_claims()), false),
            Err(ClaimError::People)
        ));
        for (claims, issuer) in [
            (person_claims(), Callers::People),
            (realm_claims(), Callers::Runs),
        ] {
            let mut no_user = claims;
            no_user.as_object_mut().unwrap().remove("kube_user");
            let p = match issuer {
                Callers::People => person(no_user),
                Callers::Runs => principal(no_user),
            };
            assert!(matches!(
                Identity::from_principal(&p, true),
                Err(ClaimError::NoKubeUser)
            ));
        }
    }

    #[test]
    fn role_and_queues_claims_are_ignored() {
        let mut claims = realm_claims();
        claims["role"] = json!("viewer");
        claims["queues"] = json!(["only-this-one"]);
        assert!(Identity::from_principal(&principal(claims), false).is_ok());
    }

    #[test]
    fn a_token_with_malformed_identity_claims_is_refused() {
        let cases = [
            ("run_id", Some(json!(7))),
            ("namespace", Some(json!("Not_A_Label"))),
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

    fn launcher_claims() -> Value {
        let mut claims = crucible_mcp_auth::testing::run_claims();
        claims["kube_user"] = json!("alice");
        claims
    }

    #[tokio::test]
    async fn a_run_token_authenticates_and_anything_else_is_challenged() {
        let (_jwks, app) = app().await;
        let token = crucible_mcp_auth::testing::sign("a", &launcher_claims());
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
        let foreign = crucible_mcp_auth::testing::sign("b", &launcher_claims());
        assert_eq!(
            post(app, Some(&format!("Bearer {foreign}"))).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}

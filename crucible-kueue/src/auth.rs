//! Request authentication: an OAuth access token from a trusted issuer. The request leaves with
//! an [`Identity`] (what the tools resolve into a [`crate::caller::Caller`] per call) and a
//! [`Principal`] (what the guard's rules see) in its extensions.
//!
//! A token stands for a run (from a run issuer, carrying `run_id`) or a person (from a people
//! issuer), and either way names the Kubernetes user it acts as (`kube_user`): the launcher for a
//! run, the person themselves otherwise. That user's RBAC decides every call; the token's
//! `namespace` claim is only a default. Which of the token's claims play those parts is a
//! [`ClaimMap`], applied before anything else reads the claims.

use crate::caller::{Identity, NameError, Namespace};
use crate::config::{self, ClaimMap, NAMESPACE_CLAIM, RUN_ID_CLAIM};
use crate::namespaces::Namespaces;
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
    pub claims: ClaimMap,
    /// Supplies the default namespace of a token that names none.
    pub namespaces: Arc<Namespaces>,
}

/// A token that names no namespace gets the one namespace its caller can use, when there is
/// exactly one, before the guard checks it and the tools read it.
async fn default_namespace(principal: &mut Principal, namespaces: &Namespaces) {
    if principal.namespace.is_some() {
        return;
    }
    if let [only] = namespaces.usable(principal).await.as_slice() {
        principal
            .claims
            .insert(NAMESPACE_CLAIM.to_string(), Value::String(only.clone()));
        principal.namespace = Some(only.clone());
    }
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
    /// The identity an access token carries, after [`ClaimMap::canonicalize`]; its issuer decides
    /// which kind. Both kinds need `kube_user`. A run's is its `run_id` and its presenter
    /// (`act.sub`) is recorded as the actor; a person's needs `people` and owns its workloads
    /// under its own subject. A subject or run id that is not a label value is sanitized with a
    /// hash. `role` and `queues` claims are ignored.
    pub fn from_principal(principal: &Principal, people: bool) -> Result<Self, ClaimError> {
        let claim = |key: &'static str| {
            principal
                .claims
                .get(key)
                .and_then(Value::as_str)
                .ok_or(ClaimError::Missing(key))
        };
        let is_person = principal.callers == Callers::People;
        if is_person && principal.claims.contains_key(RUN_ID_CLAIM) {
            return Err(ClaimError::RunFromPeopleIssuer);
        }
        if is_person && !people {
            return Err(ClaimError::People);
        }
        if !principal.claims.contains_key(USER_CLAIM) {
            return Err(ClaimError::NoKubeUser);
        }
        let kube_user = claim(USER_CLAIM)?.to_string();
        let namespace = match principal.claims.get(NAMESPACE_CLAIM) {
            None | Some(Value::Null) => None,
            Some(_) => Some(Namespace::parse(claim(NAMESPACE_CLAIM)?)?),
        };
        let (run_id, actor) = match is_person {
            true => (config::run_id_of(&principal.subject)?, None),
            false => (
                config::run_id_of(claim(RUN_ID_CLAIM)?)?,
                principal.actor_chain.first().cloned(),
            ),
        };
        Ok(Self {
            run_id,
            namespace,
            launched_by: config::launcher_of(&principal.subject)?,
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
    let mut principal = match auth.oidc.authenticate(headers).await {
        Ok(p) => p,
        Err(error) => {
            tracing::info!(%error, "token refused");
            return auth.oidc.challenge(headers, &error);
        }
    };
    auth.claims.canonicalize(&mut principal);
    default_namespace(&mut principal, &auth.namespaces).await;
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
    use crate::caller::{Caller, Launcher};
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

    #[test]
    fn a_subject_that_is_not_a_label_value_is_sanitized_not_refused() {
        let mut claims = person_claims();
        claims["sub"] = json!("alice@example.com");
        let id = Identity::from_principal(&person(claims), true).unwrap();
        assert!(
            id.launched_by.as_str().starts_with("alice-example.com-"),
            "{}",
            id.launched_by.as_str()
        );
        assert_eq!(id.run_id.as_str(), id.launched_by.as_str());
    }

    #[test]
    fn a_stock_oidc_token_maps_to_a_person_through_the_claim_map() {
        let map = ClaimMap {
            user: "preferred_username".into(),
            user_prefix: String::new(),
            groups: "groups".into(),
            run_id: None,
            namespace: "namespace".into(),
        };
        let mut p = person(json!({
            "sub": "8c1f2e9a-4b7d", "preferred_username": "alice", "groups": ["ml"],
            "scope": "kueue.read kueue.submit",
        }));
        map.canonicalize(&mut p);
        let id = Identity::from_principal(&p, true).unwrap();
        assert_eq!(id.kube_user, "alice");
        assert_eq!(id.run_id.as_str(), "8c1f2e9a-4b7d");
        assert_eq!(id.namespace, None);
        let mut nameless = person(json!({"sub": "s", "scope": "kueue.read"}));
        map.canonicalize(&mut nameless);
        assert!(matches!(
            Identity::from_principal(&nameless, true),
            Err(ClaimError::NoKubeUser)
        ));
    }

    /// Namespaces configured as `names`; the client is never reached.
    fn served(names: &[&str]) -> Namespaces {
        crate::install_tls_provider();
        let config = kube::Config::new("http://127.0.0.1:1".parse().unwrap());
        Namespaces::new(
            kube::Client::try_from(config).unwrap(),
            crate::kueue::KueueVersion::default(),
            names.iter().map(|n| n.to_string()).collect(),
            None,
        )
    }

    #[tokio::test]
    async fn a_token_without_a_namespace_defaults_to_the_only_one_its_caller_can_use() {
        let mut p = person(json!({"sub": "p", "kube_user": "alice"}));
        default_namespace(&mut p, &served(&["ml-a"])).await;
        assert_eq!(p.namespace.as_deref(), Some("ml-a"));
        assert_eq!(p.claims["namespace"], "ml-a");
        let id = Identity::from_principal(&p, true).unwrap();
        assert_eq!(id.in_namespace(None).unwrap().namespace.as_str(), "ml-a");

        let mut two = person(json!({"sub": "p", "kube_user": "alice"}));
        default_namespace(&mut two, &served(&["ml-a", "ml-b"])).await;
        assert_eq!(two.namespace, None, "two choices: the caller names one");

        let mut named = person(json!({"sub": "p", "kube_user": "alice", "namespace": "ml-b"}));
        default_namespace(&mut named, &served(&["ml-a"])).await;
        assert_eq!(
            named.namespace.as_deref(),
            Some("ml-b"),
            "a token's own namespace wins"
        );
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
            claims: ClaimMap::default(),
            namespaces: Arc::new(served(&[])),
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

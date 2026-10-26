//! The routes agent tools and people's cluster identities add. [`crate::api::router`] mounts the
//! identity routes only when people's cluster usernames matter (people OAuth, cluster links, or
//! agent tokens that carry them), and the agent tool routes only when agent tokens are minted.

use crate::api::state::ApiState;
use axum::Router;
use axum::routing::get;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::identity::api::kube_user::override_kube_user,
        crate::identity::api::cluster_identities::list_my_cluster_identities,
        crate::identity::api::cluster_identities::link_by_token,
        crate::identity::api::cluster_identities::unlink,
        crate::identity::api::cluster_identities::start_oauth_link,
        crate::identity::api::cluster_identities::oauth_link_callback,
        crate::identity::api::consent::get_consent,
        crate::identity::api::consent::decide_consent,
        crate::identity::api::mcp_sign_ins::list_mcp_sign_ins,
        crate::identity::api::mcp_sign_ins::sign_out_mcp_clients,
    ),
    components(schemas(
        crate::identity::api::cluster_identities::ClusterIdentityDto,
        crate::identity::api::cluster_identities::IdentityRuleDto,
        crate::identity::api::cluster_identities::ClaimDto,
        crate::identity::api::cluster_identities::IdentitySourceDto,
        crate::identity::api::cluster_identities::LinkStartDto,
        crate::identity::api::cluster_identities::LinkStartBody,
        crate::identity::api::cluster_identities::LinkBody,
        crate::identity::api::consent::ConsentDto,
        crate::identity::api::consent::DecisionBody,
        crate::identity::api::consent::DecisionDto,
        crate::identity::api::consent::Verdict,
        crate::identity::api::mcp_sign_ins::McpSignInDto,
        crate::identity::api::mcp_sign_ins::McpSignInsDto,
        crate::identity::api::mcp_sign_ins::SignedOutDto,
        crate::identity::api::kube_user::KubeUserDto,
        crate::identity::api::kube_user::OverrideBody,
    ))
)]
pub(crate) struct IdentityDoc;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::runs::api::runs::get_agent_tool_defaults,
        crate::runs::api::runs::get_agent_clusters,
        crate::identity::api::prefs::get_notice_prefs,
        crate::identity::api::prefs::put_notice_prefs,
    ),
    components(schemas(
        crate::runs::agent_tools::Defaults,
        crate::agent_auth::discovery::AgentCluster,
        crate::identity::api::prefs::NoticePrefsDto,
    ))
)]
pub(crate) struct AgentToolsDoc;

pub(crate) fn identity_routes(router: OpenApiRouter<ApiState>) -> OpenApiRouter<ApiState> {
    router
        .routes(routes!(crate::identity::api::kube_user::override_kube_user))
        .routes(routes!(
            crate::identity::api::cluster_identities::list_my_cluster_identities
        ))
        .routes(routes!(
            crate::identity::api::cluster_identities::link_by_token,
            crate::identity::api::cluster_identities::unlink
        ))
        .routes(routes!(
            crate::identity::api::cluster_identities::start_oauth_link
        ))
        .routes(routes!(
            crate::identity::api::cluster_identities::oauth_link_callback
        ))
        .routes(routes!(
            crate::identity::api::mcp_sign_ins::list_mcp_sign_ins,
            crate::identity::api::mcp_sign_ins::sign_out_mcp_clients
        ))
        .routes(routes!(
            crate::identity::api::consent::get_consent,
            crate::identity::api::consent::decide_consent
        ))
}

pub(crate) fn agent_tool_routes(router: Router<ApiState>) -> Router<ApiState> {
    router
        .route(
            "/api/prefs/notices",
            get(crate::identity::api::prefs::get_notice_prefs)
                .put(crate::identity::api::prefs::put_notice_prefs)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::identity::api::prefs::MAX_PREFS_BYTES,
                )),
        )
        .route(
            "/api/agent-tools",
            get(crate::runs::api::runs::get_agent_tool_defaults),
        )
        .route(
            "/api/agent-clusters",
            get(crate::runs::api::runs::get_agent_clusters),
        )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sqlx::PgPool;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct NoSink;
    impl crate::daemon::queue::OverrideSink for NoSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    /// A deployment that mints agent tokens and nothing else still serves the identity routes:
    /// people link the usernames their runs' tokens carry, and an administrator sets the override.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn agent_tokens_alone_serve_the_identity_routes(pool: PgPool) {
        let state = crate::api::state::ApiState {
            agent_tools: true,
            ..crate::api::state::ApiState::test(crate::Db::new(pool), Arc::new(NoSink))
        }
        .with_cluster_links(None)
        .with_people_issuer(None);
        assert!(state.identities);
        let res = crate::api::router(state)
            .oneshot(
                Request::get("/api/me/cluster-identities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::NOT_FOUND);
    }

    /// Switched on, each feature's routes are served and documented in the served spec.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn switched_on_the_features_routes_are_served_and_documented(pool: PgPool) {
        let state = crate::api::state::ApiState {
            agent_tools: true,
            identities: true,
            ..crate::api::state::ApiState::test(crate::Db::new(pool), Arc::new(NoSink))
        };
        let app = crate::api::router(state);
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();
        for path in [
            "/api/agent-tools",
            "/api/prefs/notices",
            "/api/users/{login}/kube-user",
            "/api/me/cluster-identities",
            "/api/me/cluster-identities/{cluster}",
            "/api/me/cluster-identities/{cluster}/link",
            "/oauth/cluster-link/{cluster}/callback",
            "/api/me/mcp-sign-ins",
            "/api/oauth/consent",
        ] {
            assert!(
                spec["paths"].get(path).is_some(),
                "{path} is not documented"
            );
        }
        let res = app
            .oneshot(
                Request::get("/api/agent-tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}

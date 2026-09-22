//! The cluster-snapshot push surface: `POST /api/clusters/push` for spokes the hub cannot reach
//! (corp-internal clusters have no inbound path from the hub, but plenty of outbound). Mounted
//! OUTSIDE the human bearer guard, like the ingest drop-box — it authenticates with its own
//! per-cluster static token (`CONTROLLER_PUSH_TOKENS`, `cluster=token[,cluster=token…]`).
//!
//! TokenReview-style validation is deliberately not used here: reviewing a spoke's ServiceAccount
//! token requires reaching that spoke's API server, which is the exact capability these clusters
//! lack. The cluster name binds to the token, never to the request body, so a leaked token for
//! one cluster cannot repaint another cluster's card.

#![allow(clippy::disallowed_macros)]

use crate::runs::cluster_stats::{ClusterSnapshot, ClusterStats};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::sync::Arc;

/// The per-cluster push credentials: token -> cluster name.
#[derive(Debug, Clone, Default)]
pub struct PushTokens(HashMap<String, String>);

impl PushTokens {
    /// Parse `CONTROLLER_PUSH_TOKENS` (`cluster=token,cluster=token`). Malformed entries are an
    /// error, not a skip — a typo'd credential list must fail deploy-loud, never silently drop a
    /// cluster's push access.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("CONTROLLER_PUSH_TOKENS") {
            Ok(raw) => Self::parse(&raw),
            Err(_) => Ok(Self::default()),
        }
    }

    fn parse(raw: &str) -> anyhow::Result<Self> {
        let mut map = HashMap::new();
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let Some((cluster, token)) = entry.split_once('=') else {
                anyhow::bail!("CONTROLLER_PUSH_TOKENS entry without '=': {entry:?}");
            };
            let (cluster, token) = (cluster.trim(), token.trim());
            if cluster.is_empty() || token.is_empty() {
                anyhow::bail!("CONTROLLER_PUSH_TOKENS entry with an empty side: {entry:?}");
            }
            if map.insert(token.to_string(), cluster.to_string()).is_some() {
                anyhow::bail!("CONTROLLER_PUSH_TOKENS reuses one token for two clusters");
            }
        }
        Ok(Self(map))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The cluster this bearer token pushes for, if it is a configured push credential.
    fn cluster_for(&self, bearer: &str) -> Option<&str> {
        self.0.get(bearer).map(String::as_str)
    }
}

#[derive(Clone)]
pub struct PushState {
    pub stats: Arc<ClusterStats>,
    pub tokens: Arc<PushTokens>,
}

/// The push surface's router. Empty token set ⇒ the route still mounts and every POST is 401 —
/// same fail-closed shape as an unset ingest credential.
pub fn router(state: PushState) -> axum::Router {
    axum::Router::new()
        .route("/api/clusters/push", axum::routing::post(push_snapshot))
        .with_state(state)
}

async fn push_snapshot(
    State(state): State<PushState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let Some(cluster) = bearer.and_then(|b| state.tokens.cluster_for(b)) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "no push credential for this bearer"})),
        )
            .into_response();
    };
    let snapshot: ClusterSnapshot = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({"error": format!("snapshot body: {e}")})),
            )
                .into_response();
        }
    };
    state.stats.record_push(cluster, snapshot).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({"cluster": cluster})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use crate::runs::cluster_push::*;
    use crate::runs::cluster_stats::GpuPool;
    use tower::ServiceExt as _;

    fn state(tokens: &str) -> PushState {
        PushState {
            stats: Arc::new(ClusterStats::new(Arc::new(
                crate::runs::clusters::ClusterClients::new(None),
            ))),
            tokens: Arc::new(PushTokens::parse(tokens).expect("tokens")),
        }
    }

    fn snapshot_body(cluster: &str) -> String {
        serde_json::json!({
            "cluster": cluster,
            "reachable": true,
            "age_secs": 0,
            "pools": [{"pool": "NVIDIA-A100-SXM4-80GB", "allocatable": 39, "requested": 31}],
            "kueue": [{"queue": "crucible", "pending": 0, "admitted": 2,
                       "gpu_nominal": 32, "gpu_reserved": 16}]
        })
        .to_string()
    }

    async fn post(state: &PushState, bearer: Option<&str>, body: String) -> axum::http::StatusCode {
        let mut req = axum::http::Request::post("/api/clusters/push")
            .header("content-type", "application/json");
        if let Some(b) = bearer {
            req = req.header("authorization", format!("Bearer {b}"));
        }
        router(state.clone())
            .oneshot(req.body(axum::body::Body::from(body)).expect("request"))
            .await
            .expect("response")
            .status()
    }

    #[test]
    fn tokens_parse_and_reject_malformed_entries() {
        let t = PushTokens::parse("pike=tok1, kelp=tok2").expect("parses");
        assert_eq!(t.cluster_for("tok1"), Some("pike"));
        assert_eq!(t.cluster_for("tok2"), Some("kelp"));
        assert_eq!(t.cluster_for("nope"), None);
        assert!(PushTokens::parse("pike").is_err(), "no '='");
        assert!(PushTokens::parse("=tok").is_err(), "empty cluster");
        assert!(PushTokens::parse("pike=").is_err(), "empty token");
        assert!(
            PushTokens::parse("a=tok,b=tok").is_err(),
            "one token, two clusters"
        );
        assert!(PushTokens::parse("").expect("empty ok").is_empty());
    }

    #[tokio::test]
    async fn push_requires_a_known_bearer() {
        let st = state("pike=sekrit");
        assert_eq!(
            post(&st, None, snapshot_body("pike")).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post(&st, Some("wrong"), snapshot_body("pike")).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post(&st, Some("sekrit"), snapshot_body("pike")).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn push_rejects_a_malformed_body_and_binds_cluster_to_the_token() {
        let st = state("pike=sekrit");
        assert_eq!(
            post(&st, Some("sekrit"), "not json".to_string()).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        // The body claims wharf; the token says pike. The token wins.
        assert_eq!(
            post(&st, Some("sekrit"), snapshot_body("wharf")).await,
            StatusCode::OK
        );
        let served = st.stats.snapshots(&[]).await;
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].cluster, "pike", "token identity, not body");
        assert!(served[0].reachable);
        assert_eq!(
            served[0].pools,
            vec![GpuPool {
                pool: "NVIDIA-A100-SXM4-80GB".into(),
                allocatable: 39,
                requested: 31
            }]
        );
        assert_eq!(
            served[0].kueue.as_deref().map(|k| k.len()),
            Some(1),
            "kueue rows ride the push"
        );
    }
}

//! Inference providers and the dispatch defaults that pick one.

use crate::client::{Client, encode};
use crate::dto;
use crate::ops::{Operation, pretty, read_body};
use crate::render;
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct Providers {
    #[arg(long)]
    pub json: bool,
}

impl Operation for Providers {
    type Output = Vec<dto::Provider>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.providers().await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::providers(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Register a provider from a JSON body (`POST /api/providers`).
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct ProviderCreate {
    #[arg(long)]
    pub file: PathBuf,
}

impl Operation for ProviderCreate {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.post("/api/providers", Some(&read_body(&self.file)?))
            .await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

/// Replace a provider's registration from a JSON body. Its id and owner stay as registered.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct ProviderUpdate {
    pub id: String,
    #[arg(long)]
    pub file: PathBuf,
}

impl Operation for ProviderUpdate {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        let path = format!("/api/providers/{}", encode(&self.id));
        c.put(&path, &read_body(&self.file)?).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct ProviderDelete {
    pub id: String,
}

impl Operation for ProviderDelete {
    type Output = ();

    async fn call(&self, c: &Client) -> Result<Value> {
        c.delete(&format!("/api/providers/{}", encode(&self.id)))
            .await?;
        Ok(Value::Null)
    }

    fn render(&self, _c: &Client, (): Self::Output) -> Result<String> {
        Ok(format!("deleted provider {}\n", self.id))
    }

    fn json(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct DispatchDefaults {
    #[arg(long)]
    pub json: bool,
}

impl Operation for DispatchDefaults {
    type Output = Vec<dto::DispatchDefault>;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut config: Value = c.get("/api/config/providers").await?;
        Ok(config["defaults"].take())
    }

    fn render(&self, _c: &Client, defaults: Self::Output) -> Result<String> {
        Ok(render::dispatch_defaults(&defaults))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Set one scope's default for one workload class from a JSON body. Admin only.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct DispatchDefaultSet {
    #[arg(long)]
    pub file: PathBuf,
}

impl Operation for DispatchDefaultSet {
    type Output = dto::DispatchDefault;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.put("/api/config/dispatch-defaults", &read_body(&self.file)?)
            .await
    }

    fn render(&self, _c: &Client, set: Self::Output) -> Result<String> {
        Ok(render::dispatch_defaults(std::slice::from_ref(&set)))
    }

    fn json(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::providers::*;
    use crate::ops::test_client;

    /// The provider and dispatch-default commands send the body file as is to the path the
    /// controller routes, and render what it answered.
    #[tokio::test]
    async fn provider_and_dispatch_default_commands_hit_their_routes() {
        use axum::http::StatusCode;
        use axum::routing::{get, put};

        let provider = serde_json::json!({
            "id": "enmaas",
            "kind": "custom",
            "harness": "claude",
            "default_model": "claude-opus-5-5",
            "protocol": "messages",
            "endpoint": "https://api.enmaas.devshift.net",
            "secret_name": "enmaas-api-key",
            "enabled": true,
            "owner": "team:tibrahim-all"
        });
        let listed = provider.clone();
        let created = provider.clone();
        let default = serde_json::json!({
            "scope_kind": "platform",
            "scope_ref": "",
            "workload_class": "playbook",
            "provider": "enmaas",
            "model": null,
            "fallback_provider": "vertex",
            "fallback_model": null
        });
        let config = serde_json::json!({ "providers": [], "defaults": [default.clone()] });
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route(
                "/api/providers",
                get(move || async move { axum::Json(serde_json::json!([listed])) }).post(
                    move |body: String| async move {
                        let sent: Value = serde_json::from_str(&body).expect("JSON");
                        assert_eq!(sent["id"], "enmaas");
                        (StatusCode::CREATED, axum::Json(created))
                    },
                ),
            )
            .route(
                "/api/providers/{id}",
                axum::routing::delete(
                    |axum::extract::Path(id): axum::extract::Path<String>| async move {
                        assert_eq!(id, "pricetag-glm");
                        StatusCode::NO_CONTENT
                    },
                )
                .put(
                    |axum::extract::Path(id): axum::extract::Path<String>, body: String| async move {
                        assert_eq!(id, "enmaas");
                        let sent: Value = serde_json::from_str(&body).expect("JSON");
                        axum::Json(sent)
                    },
                ),
            )
            .route(
                "/api/config/providers",
                get(move || async move { axum::Json(config) }),
            )
            .route(
                "/api/config/dispatch-defaults",
                put(|body: String| async move {
                    let sent: Value = serde_json::from_str(&body).expect("JSON");
                    axum::Json(sent)
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);
        let dir = tempfile::tempdir().expect("tempdir");
        let body = dir.path().join("provider.json");
        std::fs::write(&body, provider.to_string()).expect("write");
        let default_body = dir.path().join("default.json");
        std::fs::write(&default_body, default.to_string()).expect("write");

        let table = Providers { json: false }
            .run(&client)
            .await
            .expect("providers");
        assert!(table.starts_with("1 providers "), "{table}");
        assert!(table.contains("enmaas"), "{table}");
        assert!(table.contains("claude-opus-5-5"), "{table}");

        let made = ProviderCreate { file: body.clone() }
            .run(&client)
            .await
            .expect("create");
        assert!(made.contains("\"id\": \"enmaas\""), "{made}");
        let replaced = ProviderUpdate {
            id: "enmaas".into(),
            file: body,
        }
        .run(&client)
        .await
        .expect("update");
        assert!(replaced.contains("claude-opus-5-5"), "{replaced}");

        assert_eq!(
            ProviderDelete {
                id: "pricetag-glm".into()
            }
            .run(&client)
            .await
            .expect("delete"),
            "deleted provider pricetag-glm\n"
        );

        let defaults = DispatchDefaults { json: false }
            .run(&client)
            .await
            .expect("defaults");
        assert!(defaults.contains("platform"), "{defaults}");
        assert!(defaults.contains("vertex"), "{defaults}");
        let raw = DispatchDefaults { json: true }
            .run(&client)
            .await
            .expect("json");
        assert!(
            raw.trim_start().starts_with('['),
            "only the defaults: {raw}"
        );

        let set = DispatchDefaultSet { file: default_body }
            .run(&client)
            .await
            .expect("set");
        assert!(set.starts_with("1 dispatch defaults\n"), "{set}");
        assert!(set.contains("enmaas"), "{set}");
    }
}

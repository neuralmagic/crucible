//! Secrets: what the caller owns, registering one, and binding it to the scope a run reads it at.

use crate::client::Client;
use crate::dto;
use crate::ops::{Operation, put_trimmed};
use crate::render;
use anyhow::{Result, bail};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Secrets {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Secrets {
    type Output = Vec<dto::Secret>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.secrets().await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::secrets(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// What a registered value is, which decides how a binding may project it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SecretKind {
    Opaque,
    File,
    RegistryAuthfile,
    Kubeconfig,
    InferenceApiKey,
}

impl SecretKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Opaque => "opaque",
            Self::File => "file",
            Self::RegistryAuthfile => "registry_authfile",
            Self::Kubeconfig => "kubeconfig",
            Self::InferenceApiKey => "inference_api_key",
        }
    }
}

/// Whether the agent inside the sandbox may see the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Visibility {
    BrokerOnly,
    AgentVisible,
}

impl Visibility {
    fn as_str(self) -> &'static str {
        match self {
            Self::BrokerOnly => "broker_only",
            Self::AgentVisible => "agent_visible",
        }
    }
}

/// A secret's bytes. No `Debug` and no `Display`, so it cannot be formatted into output or a log.
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(raw: String) -> Result<Self> {
        let value = raw.trim_end_matches(['\r', '\n']).to_string();
        if value.trim().is_empty() {
            bail!("the secret value on stdin is empty");
        }
        Ok(Self(value))
    }
}

/// Register a secret. `visibility` and `owner` absent take the controller's defaults:
/// `broker_only`, and the caller. The CLI fills one from its flags and the value on stdin.
pub struct SecretCreate {
    pub name: String,
    pub kind: SecretKind,
    pub visibility: Option<Visibility>,
    pub owner: Option<String>,
    pub value: SecretValue,
    pub json: bool,
}

impl Operation for SecretCreate {
    type Output = dto::Secret;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut body = serde_json::json!({
            "name": self.name,
            "kind": self.kind.as_str(),
            "value": self.value.0,
        });
        put_trimmed(
            &mut body,
            "visibility",
            self.visibility.map(Visibility::as_str),
        );
        put_trimmed(&mut body, "owner", self.owner.as_deref());
        c.create_secret(&body).await
    }

    fn render(&self, _c: &Client, created: Self::Output) -> Result<String> {
        Ok(render::secret_created(&created))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Bind a secret to a scope so a run there receives it. The CLI fills one from its scope and
/// projection flags.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct SecretBind {
    /// The secret's id, as crucible_secrets lists it.
    pub secret_id: String,
    /// `repo`, `playbook`, or `domain`. A draft is a `playbook` scope under its draft id.
    pub scope_kind: String,
    pub scope_id: String,
    /// `env` or `file`.
    pub projection_kind: String,
    /// The environment variable name, or the absolute file path.
    pub projection: String,
    /// The manifest's `[[secret]] name` this binding satisfies. Omit for the secret's own name.
    #[serde(default)]
    pub declared_name: Option<String>,
    #[serde(default)]
    pub json: bool,
}

impl Operation for SecretBind {
    type Output = dto::SecretBinding;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut body = serde_json::json!({
            "scope_kind": self.scope_kind,
            "scope_id": self.scope_id,
            "projection_kind": self.projection_kind,
            "projection": self.projection,
        });
        put_trimmed(&mut body, "declared_name", self.declared_name.as_deref());
        c.bind_secret(&self.secret_id, &body).await
    }

    fn render(&self, _c: &Client, bound: Self::Output) -> Result<String> {
        Ok(render::secret_bound(&bound))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::secrets::*;
    use crate::ops::test_client;

    #[test]
    fn a_secret_value_drops_only_the_trailing_newline() {
        assert_eq!(
            SecretValue::new("ghp_abc\n".into()).expect("value").0,
            "ghp_abc"
        );
        assert_eq!(
            SecretValue::new("ghp_abc\r\n".into()).expect("value").0,
            "ghp_abc"
        );
        assert_eq!(
            SecretValue::new("  a b  \n\n".into()).expect("value").0,
            "  a b  ",
            "inner and leading whitespace is part of the value"
        );
        assert!(SecretValue::new(String::new()).is_err());
        assert!(SecretValue::new(" \n".into()).is_err());
    }

    #[test]
    fn secret_enums_send_the_controller_spelling() {
        assert_eq!(SecretKind::Opaque.as_str(), "opaque");
        assert_eq!(SecretKind::File.as_str(), "file");
        assert_eq!(SecretKind::RegistryAuthfile.as_str(), "registry_authfile");
        assert_eq!(SecretKind::Kubeconfig.as_str(), "kubeconfig");
        assert_eq!(SecretKind::InferenceApiKey.as_str(), "inference_api_key");
        assert_eq!(Visibility::BrokerOnly.as_str(), "broker_only");
        assert_eq!(Visibility::AgentVisible.as_str(), "agent_visible");
    }

    /// Listing is metadata only, and a bind sends exactly the scope and projection asked for,
    /// with the declared name only when one was given.
    #[tokio::test]
    async fn secrets_and_bind_speak_their_routes() {
        use axum::extract::Path as AxumPath;
        use axum::http::StatusCode;
        use axum::routing::{get, post};

        async fn list() -> (StatusCode, String) {
            (
                StatusCode::OK,
                serde_json::json!([{
                    "id": "01a0",
                    "name": "gh-token",
                    "owner": "user:wynn",
                    "kind": "opaque",
                    "visibility": "agent_visible",
                    "consumer": "run",
                    "mode": "managed",
                    "vault_path": "secret/x",
                    "created_by": "wynn",
                    "created_at": "2026-08-31T20:15:07Z",
                    "updated_at": "2026-08-31T20:15:07Z"
                }])
                .to_string(),
            )
        }

        async fn bind(AxumPath(id): AxumPath<String>, body: String) -> (StatusCode, String) {
            assert_eq!(id, "01a0");
            let sent: Value = serde_json::from_str(&body).expect("a JSON body");
            assert_eq!(
                sent,
                serde_json::json!({
                    "scope_kind": "playbook",
                    "scope_id": "docs-drift",
                    "projection_kind": "env",
                    "projection": "GH_TOKEN",
                    "declared_name": "docs-drift-pr-token",
                }),
                "nothing beyond what was asked for, and no absent declared_name"
            );
            (
                StatusCode::CREATED,
                serde_json::json!({
                    "id": "b1",
                    "secret_id": "01a0",
                    "scope_kind": "playbook",
                    "scope_id": "docs-drift",
                    "projection_kind": "env",
                    "projection": "GH_TOKEN",
                    "declared_name": "docs-drift-pr-token",
                    "created_by": "wynn",
                    "created_at": "2026-08-31T20:16:00Z"
                })
                .to_string(),
            )
        }

        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/secrets", get(list))
            .route("/api/secrets/{id}/bindings", post(bind));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let listed = Secrets { json: false }.run(&client).await.expect("list");
        assert!(listed.contains("1 secrets"), "{listed}");
        assert!(listed.contains("01a0"), "{listed}");
        assert!(listed.contains("agent_visible"), "{listed}");
        assert!(
            !listed.contains("secret/x"),
            "no vault path in the listing: {listed}"
        );

        let bound = SecretBind {
            secret_id: "01a0".into(),
            scope_kind: "playbook".into(),
            scope_id: "docs-drift".into(),
            projection_kind: "env".into(),
            projection: "GH_TOKEN".into(),
            declared_name: Some("docs-drift-pr-token".into()),
            json: false,
        }
        .run(&client)
        .await
        .expect("bind");
        assert!(
            bound.contains("bound 01a0 to playbook/docs-drift: accepted"),
            "{bound}"
        );
        assert!(
            bound.contains("as docs-drift-pr-token -> env GH_TOKEN"),
            "{bound}"
        );
        assert!(bound.contains("actor: wynn"), "{bound}");
    }

    /// A create sends the name, kind, and value, adds visibility and owner only when given, and
    /// prints the metadata the controller answered with, never the value.
    #[tokio::test]
    async fn secret_create_posts_the_value_and_prints_only_metadata() {
        use axum::http::StatusCode;
        use axum::routing::post;

        async fn create(body: String) -> (StatusCode, String) {
            let sent: Value = serde_json::from_str(&body).expect("a JSON body");
            let expected = if sent["name"] == "bare" {
                serde_json::json!({ "name": "bare", "kind": "opaque", "value": "ghp_secret" })
            } else {
                serde_json::json!({
                    "name": "bugwatch-label-token",
                    "kind": "opaque",
                    "value": "ghp_secret",
                    "visibility": "broker_only",
                    "owner": "user:wynn",
                })
            };
            assert_eq!(sent, expected, "exactly the fields asked for");
            (
                StatusCode::CREATED,
                serde_json::json!({
                    "id": "01b0",
                    "name": sent["name"],
                    "owner": "user:wynn",
                    "kind": "opaque",
                    "visibility": "broker_only",
                    "consumer": "run",
                    "mode": "managed",
                    "vault_path": "secret/x",
                    "current_version": 1,
                    "created_by": "wynn",
                    "created_at": "2026-09-29T16:00:00Z",
                    "updated_at": "2026-09-29T16:00:00Z"
                })
                .to_string(),
            )
        }

        let app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .route("/api/secrets", post(create));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let printed = SecretCreate {
            name: "bugwatch-label-token".into(),
            kind: SecretKind::Opaque,
            visibility: Some(Visibility::BrokerOnly),
            owner: Some(" user:wynn ".into()),
            value: SecretValue::new("ghp_secret\n".into()).expect("value"),
            json: false,
        }
        .run(&client)
        .await
        .expect("create");
        assert!(
            printed.starts_with("registered bugwatch-label-token as 01b0\n"),
            "{printed}"
        );
        assert!(printed.contains("visibility: broker_only"), "{printed}");
        assert!(printed.contains("crux secret-bind 01b0"), "{printed}");
        assert!(
            !printed.contains("ghp_secret"),
            "never the value: {printed}"
        );
        assert!(
            !printed.contains("secret/x"),
            "never the vault path: {printed}"
        );

        let bare = SecretCreate {
            name: "bare".into(),
            kind: SecretKind::Opaque,
            visibility: None,
            owner: None,
            value: SecretValue::new("ghp_secret".into()).expect("value"),
            json: true,
        }
        .run(&client)
        .await
        .expect("create");
        assert!(bare.contains("\"id\": \"01b0\""), "{bare}");
    }
}

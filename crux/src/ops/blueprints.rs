//! Blueprints: registered playbooks offered as starting points for new drafts.

use crate::client::Client;
use crate::dto;
use crate::ops::Operation;
use crate::ops::drafts::studio_url;
use crate::render;
use anyhow::Result;
use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value;

/// List the blueprints the caller can read, with how each declared secret would reach them.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Blueprints {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Blueprints {
    type Output = Vec<dto::Blueprint>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.blueprints().await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::blueprints(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Start a draft from a blueprint. A reference-mode secret carries only with the caller's Vault
/// token, which this client never sends, so it comes back as a placeholder.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct BlueprintUse {
    /// The blueprint's id, as `blueprints` lists it.
    #[schemars(description = "The blueprint's id, as crucible_blueprints lists it.")]
    pub id: String,
    /// The new draft's id.
    #[serde(rename = "draft_id")]
    #[schemars(description = "The new draft's id: a lowercase slug no draft or playbook holds.")]
    pub to: String,
    /// Defaults to the blueprint's.
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Omit for the blueprint's.")]
    pub description: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for BlueprintUse {
    type Output = dto::BlueprintUsed;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.use_blueprint(&self.id, &self.to, self.description.as_deref())
            .await
    }

    fn render(&self, c: &Client, used: Self::Output) -> Result<String> {
        Ok(render::blueprint_used(
            &self.id,
            &used,
            &studio_url(c, &used.id),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Offer a registered playbook as a blueprint.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct BlueprintMark {
    /// Playbook id.
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

impl Operation for BlueprintMark {
    type Output = dto::BlueprintState;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.set_blueprint(&self.id, true).await
    }

    fn render(&self, _c: &Client, state: Self::Output) -> Result<String> {
        Ok(render::blueprint_state(&state))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Withdraw a blueprint, public or not.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct BlueprintUnmark {
    /// Playbook id.
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

impl Operation for BlueprintUnmark {
    type Output = dto::BlueprintState;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.set_blueprint(&self.id, false).await
    }

    fn render(&self, _c: &Client, state: Self::Output) -> Result<String> {
        Ok(render::blueprint_state(&state))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Let every user read and use a blueprint.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct BlueprintPublic {
    /// Playbook id.
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

impl Operation for BlueprintPublic {
    type Output = dto::BlueprintState;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.set_blueprint_public(&self.id, true).await
    }

    fn render(&self, _c: &Client, state: Self::Output) -> Result<String> {
        Ok(render::blueprint_state(&state))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Make a public blueprint private again.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct BlueprintPrivate {
    /// Playbook id.
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

impl Operation for BlueprintPrivate {
    type Output = dto::BlueprintState;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.set_blueprint_public(&self.id, false).await
    }

    fn render(&self, _c: &Client, state: Self::Output) -> Result<String> {
        Ok(render::blueprint_state(&state))
    }

    fn json(&self) -> bool {
        self.json
    }
}

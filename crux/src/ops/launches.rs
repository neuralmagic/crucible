//! Standing launches: schedules, tracker watches, and webhooks.

use crate::client::{Client, encode};
use crate::dto;
use crate::ops::{Operation, pretty, read_body};
use crate::render;
use anyhow::{Result, bail};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Schedules {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Schedules {
    type Output = Vec<dto::Schedule>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.schedules().await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::schedules(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Fire a schedule now instead of at its next window. A real firing: its run advances the
/// cursor.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct ScheduleFire {
    /// The schedule's id, as `schedules` lists it.
    #[schemars(description = "The schedule's id, as schedules lists it.")]
    pub id: String,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for ScheduleFire {
    type Output = dto::Schedule;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.fire_schedule(&self.id).await
    }

    fn render(&self, _c: &Client, schedule: Self::Output) -> Result<String> {
        Ok(format!(
            "fire {}: due now\nplaybook: {}\n",
            schedule.id, schedule.playbook
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Launch a schedule once without moving it: the run gets the stored cursor, or the one given
/// here, and never writes it back.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct ScheduleTestFire {
    /// The schedule's id, as `schedules` lists it.
    #[schemars(description = "The schedule's id, as schedules lists it.")]
    pub id: String,
    /// Give the firing this cursor value in place of the stored one.
    #[arg(long, conflicts_with = "no_cursor")]
    #[serde(default)]
    #[schemars(description = "A cursor value to give the firing in place of the stored one.")]
    pub cursor: Option<String>,
    /// Give the firing no cursor, as on the schedule's first firing.
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Give the firing no cursor, as on the schedule's first firing.")]
    pub no_cursor: bool,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl ScheduleTestFire {
    fn test_cursor(&self) -> Result<dto::TestCursor> {
        match (&self.cursor, self.no_cursor) {
            (Some(_), true) => bail!("give a cursor or no_cursor, not both"),
            (Some(value), false) => Ok(dto::TestCursor::Value {
                value: value.clone(),
            }),
            (None, true) => Ok(dto::TestCursor::None),
            (None, false) => Ok(dto::TestCursor::Stored),
        }
    }
}

impl Operation for ScheduleTestFire {
    type Output = dto::TestFire;

    async fn call(&self, c: &Client) -> Result<Value> {
        let fired = c.test_fire_schedule(&self.id, &self.test_cursor()?).await?;
        Ok(serde_json::to_value(fired)?)
    }

    fn render(&self, _c: &Client, fired: Self::Output) -> Result<String> {
        let cursor = match &fired.cursor {
            dto::TestCursor::Stored => "stored",
            dto::TestCursor::None => "none",
            dto::TestCursor::Value { .. } => "given",
        };
        Ok(format!(
            "test-fire {}: launched {}\ncursor: {cursor}\n",
            fired.schedule, fired.key
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Watches {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Watches {
    type Output = Vec<dto::Watch>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.watches().await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::watches(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Watch {
    #[schemars(description = "Watch id, as `crucible_watches` lists it.")]
    pub id: String,
}

impl Operation for Watch {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.watch(&self.id).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Webhooks {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Webhooks {
    type Output = Vec<dto::Webhook>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.webhooks().await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::webhooks(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Webhook {
    #[schemars(description = "Webhook id, as `crucible_webhooks` lists it.")]
    pub id: String,
}

impl Operation for Webhook {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.webhook(&self.id).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct WebhookDeliveries {
    #[schemars(description = "Webhook id, as `crucible_webhooks` lists it.")]
    pub id: String,
    /// Only deliveries recorded before this delivery id: the last id of the previous page.
    #[arg(long)]
    #[serde(skip)]
    pub before: Option<String>,
    /// Page size, at most 200.
    #[arg(long)]
    #[serde(skip)]
    pub limit: Option<i64>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for WebhookDeliveries {
    type Output = Vec<dto::WebhookDelivery>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.webhook_deliveries(&self.id, self.before.as_deref(), self.limit)
            .await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::webhook_deliveries(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookPresets {}

impl Operation for WebhookPresets {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.get("/api/webhooks/presets").await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

/// Check a transform the way a save does. Clean prints `ok`; otherwise one
/// `field:line:col: message` per refusal, and the command fails so a script can gate on it.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookCheck {
    #[arg(long)]
    pub file: PathBuf,
}

impl Operation for WebhookCheck {
    type Output = dto::WebhookCheck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.post("/api/webhooks/check", Some(&read_body(&self.file)?))
            .await
    }

    fn render(&self, _c: &Client, checked: Self::Output) -> Result<String> {
        let out = render::webhook_check(&checked);
        if checked.diagnostics.is_empty() {
            Ok(out)
        } else {
            bail!("{}", out.trim_end())
        }
    }

    fn json(&self) -> bool {
        false
    }
}

/// Create a webhook from a JSON body. The response carries the secret, which the controller
/// never shows again.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookCreate {
    #[arg(long)]
    pub file: PathBuf,
}

impl Operation for WebhookCreate {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.post("/api/webhooks", Some(&read_body(&self.file)?)).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookUpdate {
    pub id: String,
    #[arg(long)]
    pub file: PathBuf,
}

impl Operation for WebhookUpdate {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        let path = format!("/api/webhooks/{}", encode(&self.id));
        c.put(&path, &read_body(&self.file)?).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

async fn set_enabled(c: &Client, id: &str, enabled: bool) -> Result<Value> {
    let path = format!("/api/webhooks/{}/enabled", encode(id));
    let body = serde_json::json!({ "enabled": enabled });
    c.post(&path, Some(&body)).await
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookResume {
    pub id: String,
}

impl Operation for WebhookResume {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        set_enabled(c, &self.id, true).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookPause {
    pub id: String,
}

impl Operation for WebhookPause {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        set_enabled(c, &self.id, false).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookRotate {
    pub id: String,
}

impl Operation for WebhookRotate {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        let path = format!("/api/webhooks/{}/secret", encode(&self.id));
        c.post(&path, None::<&Value>).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookDelete {
    pub id: String,
}

impl Operation for WebhookDelete {
    type Output = ();

    async fn call(&self, c: &Client) -> Result<Value> {
        c.delete(&format!("/api/webhooks/{}", encode(&self.id)))
            .await?;
        Ok(Value::Null)
    }

    fn render(&self, _c: &Client, (): Self::Output) -> Result<String> {
        Ok(format!("deleted webhook {}\n", self.id))
    }

    fn json(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct WebhookPreview {
    #[arg(long)]
    pub file: PathBuf,
}

impl Operation for WebhookPreview {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.post("/api/webhooks/preview", Some(&read_body(&self.file)?))
            .await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::dto::TestCursor;
    use crate::ops::launches::ScheduleTestFire;
    use clap::Parser;

    #[derive(Parser)]
    struct Argv {
        #[command(flatten)]
        op: ScheduleTestFire,
    }

    fn parse(argv: &[&str]) -> Result<ScheduleTestFire, clap::Error> {
        Argv::try_parse_from(std::iter::once("crux").chain(argv.iter().copied())).map(|a| a.op)
    }

    #[test]
    fn a_test_firing_takes_the_stored_cursor_unless_told_otherwise() {
        let cursor = |argv: &[&str]| parse(argv).expect("argv").test_cursor().expect("cursor");
        assert_eq!(cursor(&["s1"]), TestCursor::Stored);
        assert_eq!(cursor(&["s1", "--no-cursor"]), TestCursor::None);
        assert_eq!(
            cursor(&["s1", "--cursor", "2026-01-01T00:00:00Z"]),
            TestCursor::Value {
                value: "2026-01-01T00:00:00Z".to_string()
            }
        );
    }

    #[test]
    fn a_given_cursor_and_no_cursor_are_refused_together() {
        assert!(parse(&["s1", "--cursor", "x", "--no-cursor"]).is_err());
        let both = ScheduleTestFire {
            id: "s1".to_string(),
            cursor: Some("x".to_string()),
            no_cursor: true,
            json: false,
        };
        assert!(both.test_cursor().is_err());
    }
}

//! Issues and the controller's decisions about them: listing, adopting, parking, and the
//! approval gate.

use crate::client::{AdoptBody, Client};
use crate::dto;
use crate::ops::{Operation, pretty};
use crate::render;
use anyhow::Result;
use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value;
use std::str::FromStr;

/// Rows `issues` prints when the caller names no limit. Applied client-side: `GET /api/issues`
/// takes no `limit` of its own.
pub const ISSUES_LIMIT_DEFAULT: i64 = 50;

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Issues {
    /// Input kind: github | scenario | jira.
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Input kind: `github`, `scenario`, or `jira`. Omit for all.")]
    pub kind: Option<String>,
    /// Status: new | scoped | awaiting-approval | building | running | pr-open | parked | done.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Status, one of `new`, `scoped`, `awaiting-approval`, `building`, `running`, `pr-open`,\n`parked`, `done`. Omit for all."
    )]
    pub status: Option<String>,
    /// Max rows. Default 50, applied client-side.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Max rows. Default 50, applied by this server: the controller returns every match."
    )]
    pub limit: Option<i64>,
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Return the controller's raw JSON instead of the compact table.")]
    pub json: bool,
}

impl Issues {
    fn cap(&self) -> usize {
        usize::try_from(self.limit.unwrap_or(ISSUES_LIMIT_DEFAULT).max(0)).unwrap_or(usize::MAX)
    }
}

impl Operation for Issues {
    type Output = Vec<dto::Issue>;

    async fn call(&self, c: &Client) -> Result<Value> {
        let status = self
            .status
            .as_deref()
            .map(dto::IssueStatus::from_str)
            .transpose()?
            .map(dto::IssueStatus::wire);
        c.issues(self.kind.as_deref(), status).await
    }

    fn render(&self, _c: &Client, mut list: Self::Output) -> Result<String> {
        let total = list.len();
        list.truncate(self.cap());
        Ok(render::issues(&list, total))
    }

    fn json(&self) -> bool {
        self.json
    }

    async fn run(&self, c: &Client) -> Result<String> {
        let mut raw = self.call(c).await?;
        if self.json() {
            if let Some(rows) = raw.as_array_mut() {
                rows.truncate(self.cap());
            }
            return pretty(&raw);
        }
        self.render(c, serde_json::from_value(raw)?)
    }
}

fn default_body_max() -> usize {
    6000
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Issue {
    /// Issue key, e.g. owner/repo#12 or scenario:<uuid>. Encoding is handled for you.
    #[schemars(description = "Issue key, e.g. `owner/repo#12` or `scenario:<uuid>`.")]
    pub key: String,
    /// Cut the issue body at this many chars.
    #[arg(long, default_value_t = 6000)]
    #[serde(default = "default_body_max")]
    #[schemars(description = "Cut the issue body at this many chars. Default 6000.")]
    pub body_max: usize,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Issue {
    type Output = dto::IssueDetail;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.issue(&self.key).await
    }

    fn render(&self, _c: &Client, detail: Self::Output) -> Result<String> {
        Ok(render::issue(&detail, self.body_max))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Adopt a scenario: a controller issue with no upstream. The CLI fills one from its flags
/// after reading the body from a file or stdin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct Adopt {
    /// One-line title.
    pub title: String,
    /// The full ask, as prose. This is what the scoping turn reads.
    pub body: String,
    /// Repos this touches. The first is the clone target.
    pub affected_repos: Vec<String>,
    /// Why the controller should spend money on this.
    pub justification: String,
    /// Whether the body is authoritative (skips grounded ranking).
    #[serde(default)]
    pub authoritative: bool,
    /// Branch or tag to clone. Omit for the repo's default branch.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// A configured broker contract name (see `crucible_contracts`). Omit for local measure.
    #[serde(default)]
    pub codegen_contract: Option<String>,
    #[serde(default)]
    pub json: bool,
}

impl Operation for Adopt {
    type Output = dto::ScenarioAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.adopt(&AdoptBody {
            title: self.title.clone(),
            body: self.body.clone(),
            affected_repos: self.affected_repos.clone(),
            justification: self.justification.clone(),
            authoritative: self.authoritative,
            git_ref: self.git_ref.clone(),
            codegen_contract: self.codegen_contract.clone(),
        })
        .await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        let mut detail = format!(
            "tier: {}\nrepos: {}",
            ack.tier,
            ack.affected_repos.join(", ")
        );
        // Echo back what the controller stored, not what was sent: it trims and validates both,
        // and a silently-normalized ref is worth seeing before a turn clones the wrong branch.
        if let Some(r) = &ack.git_ref {
            detail.push_str(&format!("\ngit_ref: {r}"));
        }
        if let Some(cc) = &ack.codegen_contract {
            detail.push_str(&format!("\ncodegen_contract: {cc}"));
        }
        Ok(render::ack(
            "adopt",
            &ack.key,
            ack.actor.as_deref(),
            &detail,
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

// Each mutation renders the controller's ack with the `actor` it recorded. The acting identity
// is not a courtesy: a request that reached the controller without identity headers books the
// change to `anonymous`, and the ack is the only place that becomes visible before someone goes
// looking in the event log a week later.

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Park {
    pub key: String,
    #[arg(long)]
    #[schemars(description = "Why. Required, and it lands in the event log against your name.")]
    pub reason: String,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Park {
    type Output = dto::OverrideAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.park(&self.key, &self.reason).await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(render::ack(
            "park",
            &ack.key,
            ack.actor.as_deref(),
            &format!("reason: {}", self.reason),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Unpark {
    pub key: String,
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Why it should run again.")]
    pub reason: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Unpark {
    type Output = dto::OverrideAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.unpark(&self.key, self.reason.as_deref()).await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(render::ack(
            "unpark",
            &ack.key,
            ack.actor.as_deref(),
            &self
                .reason
                .as_ref()
                .map(|r| format!("reason: {r}"))
                .unwrap_or_default(),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Bump {
    pub key: String,
    #[arg(long, default_value_t = 100)]
    #[schemars(description = "The new priority. Higher runs sooner.")]
    pub priority: i64,
    #[arg(long)]
    #[serde(default)]
    pub reason: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Bump {
    type Output = dto::OverrideAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.bump(&self.key, self.priority, self.reason.as_deref())
            .await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(render::ack(
            "bump",
            &ack.key,
            ack.actor.as_deref(),
            &format!("priority: {}", ack.priority.unwrap_or(self.priority)),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// `POST /api/reconcile` takes no key: it wakes the daemon's next full pass. Repeated calls
/// coalesce, and it returns before the pass runs.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Reconcile {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Reconcile {
    type Output = dto::ReconcileAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.reconcile().await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(render::ack(
            "reconcile",
            "(all issues)",
            ack.actor.as_deref(),
            "the daemon will pick this up on its next pass; it does not block",
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Redispatch {
    pub key: String,
    #[arg(long)]
    #[schemars(description = "Why this is worth the spend. Recorded against your identity.")]
    pub justification: String,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Redispatch {
    type Output = dto::OverrideAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.redispatch(&self.key, &self.justification).await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(render::ack(
            "redispatch",
            &ack.key,
            ack.actor.as_deref(),
            &format!("justification: {}", self.justification),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Approve {
    #[schemars(description = "Issue key.")]
    pub key: String,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Approve {
    type Output = dto::ApproveAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.approve(&self.key).await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(render::ack(
            "approve",
            &ack.key,
            Some(ack.approved_by.as_str()),
            &format!("scope {} approved at {}", ack.scope_id, ack.approved_at),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// The approval PR and pack digest for one issue, read out of its scopes.
///
/// `GET /api/approvals` only lists what is currently awaiting a human, so an issue whose approval
/// has already been walked through would simply be absent from it. This reads the issue instead,
/// which answers "what is the approval, and did anyone use it" for any issue at any point in its
/// life.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct ApprovalDetail {
    #[schemars(description = "Issue key.")]
    pub key: String,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for ApprovalDetail {
    type Output = dto::IssueDetail;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.issue(&self.key).await
    }

    fn render(&self, _c: &Client, detail: Self::Output) -> Result<String> {
        Ok(render::approval_detail(&self.key, &detail))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Approvals {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Approvals {
    type Output = dto::Approvals;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.approvals().await
    }

    fn render(&self, _c: &Client, d: Self::Output) -> Result<String> {
        Ok(render::approvals(&d))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Contracts {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Contracts {
    type Output = dto::BrokerContracts;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.contracts().await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::contracts(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// The controller has no issue filter on `/api/turns`, so it is applied here. Doing it
/// client-side is honest about the cost (the full page comes back either way) and keeps the
/// filter available today rather than after a controller change.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Turns {
    /// Only turns for this issue key.
    #[arg(long)]
    #[serde(default)]
    pub issue: Option<String>,
    /// Work kind: grounded-rank | scope | run.
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Work kind: `grounded-rank`, `scope`, or `run`.")]
    pub kind: Option<String>,
    /// Lifecycle state: queued | running | succeeded | failed | collected | swept.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Lifecycle state: `queued`, `running`, `succeeded`, `failed`, `collected`, `swept`."
    )]
    pub state: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Turns {
    type Output = Vec<dto::Turn>;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut raw: Value = c.turns(self.kind.as_deref(), self.state.as_deref()).await?;
        if let (Some(key), Some(rows)) = (self.issue.as_deref(), raw.as_array_mut()) {
            rows.retain(|t| t.get("issue_key").and_then(Value::as_str) == Some(key));
        }
        Ok(raw)
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::turns(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Turn {
    /// The turn's pod name.
    pub pod: String,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Turn {
    type Output = dto::Turn;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.turn(&self.pod).await
    }

    fn render(&self, _c: &Client, t: Self::Output) -> Result<String> {
        Ok(render::turn(&t))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::issues::*;
    use crate::ops::test_client;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn an_issue_list_is_capped_client_side_and_says_what_it_cut() {
        use axum::extract::{Query as AxumQuery, State};
        use axum::http::StatusCode;
        use axum::routing::get;
        use std::sync::Arc;
        use std::sync::Mutex;

        type Seen = Arc<Mutex<Vec<BTreeMap<String, String>>>>;

        async fn issues_route(
            State(seen): State<Seen>,
            AxumQuery(q): AxumQuery<BTreeMap<String, String>>,
        ) -> (StatusCode, String) {
            seen.lock().expect("lock").push(q);
            let rows: Vec<Value> = (0..120)
                .map(|n| {
                    serde_json::json!({
                        "key": format!("o/r#{n}"),
                        "kind": {"type": "github", "owner": "o", "repo": "r", "number": n},
                        "status": "parked",
                        "title": "t",
                    })
                })
                .collect();
            (StatusCode::OK, serde_json::Value::Array(rows).to_string())
        }

        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/issues", get(issues_route))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);
        let list = |status: Option<&str>, limit: Option<i64>, json: bool| Issues {
            kind: None,
            status: status.map(str::to_string),
            limit,
            json,
        };

        let capped = list(None, None, false).run(&client).await.expect("list");
        assert_eq!(
            capped.lines().next().expect("a header"),
            "50 of 120 issues (raise limit for more)"
        );
        assert_eq!(capped.lines().count(), 52);

        let asked = list(Some("pr-open"), Some(3), false)
            .run(&client)
            .await
            .expect("list");
        assert_eq!(
            asked.lines().next().expect("a header"),
            "3 of 120 issues (raise limit for more)"
        );
        assert_eq!(
            seen.lock().expect("lock").last().expect("a request")["status"],
            "pr-open"
        );

        let raw = list(None, Some(2), true).run(&client).await.expect("json");
        let parsed: Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(parsed.as_array().expect("an array").len(), 2);

        let requests_before = seen.lock().expect("lock").len();
        let refused = list(Some("scoping"), None, false)
            .run(&client)
            .await
            .expect_err("`scoping` is not a controller status");
        let msg = format!("{refused:#}");
        assert!(msg.contains("scoped"), "{msg}");
        assert!(msg.contains("awaiting-approval"), "{msg}");
        assert_eq!(
            seen.lock().expect("lock").len(),
            requests_before,
            "an unknown status never reaches the controller"
        );
    }
}

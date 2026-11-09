//! Registered playbooks: what can be launched, what a launch is held to, and the launches made.

use crate::client::{Client, PackImportSource, encode};
use crate::dto;
use crate::ops::{Operation, pretty, put_trimmed};
use crate::render;
use anyhow::Result;
use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Playbooks {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Playbooks {
    type Output = Vec<dto::Playbook>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.playbooks().await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::playbooks(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct PlaybookCaps {
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for PlaybookCaps {
    type Output = dto::PlaybookCaps;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.playbook_caps().await
    }

    fn render(&self, _c: &Client, caps: Self::Output) -> Result<String> {
        Ok(render::playbook_caps(&caps))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// The parameter schema, always as JSON: it IS a JSON Schema, and an agent about to fill one in
/// needs the types and the enums, not a prose summary of them.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct PlaybookSchema {
    /// Playbook id.
    #[schemars(description = "Playbook id, as `crucible_playbooks` lists it.")]
    pub id: String,
}

impl Operation for PlaybookSchema {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.playbook_schema(&self.id).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

pub(crate) fn parse_json(s: &str) -> Result<Value, String> {
    serde_json::from_str(s).map_err(|e| format!("not valid JSON: {e}"))
}

/// A launch ack or record with the link that opens it in the desktop tracker beside its key.
pub(crate) fn with_tracker_url(mut launch: Value) -> Value {
    let url = match (
        launch.get("playbook").and_then(Value::as_str),
        launch.get("key").and_then(Value::as_str),
    ) {
        (Some(playbook), Some(key)) if !playbook.is_empty() => {
            crate::client::tracker_url(playbook, key)
        }
        _ => return launch,
    };
    if let Some(obj) = launch.as_object_mut() {
        obj.insert("tracker_url".to_string(), Value::String(url));
    }
    launch
}

/// A launch that reruns a finished run of the same pack from some of its tasks.
pub(crate) fn put_rerun(body: &mut Value, rerun_of: Option<&str>, from: &[String]) {
    if let Some(of) = rerun_of {
        body["rerun"] = serde_json::json!({ "of": of, "from": from });
    }
}

/// Launch a playbook. The caps are the ceilings the launch is authorized against, and the
/// controller refuses anything above the admin's own. An optional field left blank is omitted
/// from the body, so the controller's own default applies.
#[derive(Debug, Clone, PartialEq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Launch {
    /// Playbook id.
    #[schemars(description = "Playbook id, as `crucible_playbooks` lists it.")]
    pub id: String,
    /// The params object its schema accepts, as JSON.
    #[arg(long, default_value = "{}", value_parser = parse_json)]
    #[serde(default)]
    #[schemars(
        description = "The parameters object its schema accepts. Read `crucible_playbook_schema` first."
    )]
    pub params: Value,
    /// Dollar ceiling for this launch.
    #[arg(long)]
    #[schemars(
        description = "Dollar ceiling for this launch. The controller refuses anything above the admin cap."
    )]
    pub max_cost: f64,
    /// Wall-clock ceiling, e.g. 30m, 2h.
    #[arg(long)]
    #[schemars(description = "Wall-clock ceiling, e.g. `30m`, `2h`.")]
    pub max_time: String,
    /// Cluster to dispatch onto. Omit for the controller's default.
    #[arg(long)]
    #[serde(default)]
    pub dispatch_target: Option<String>,
    /// Registered inference provider the run's agent talks to, replacing the pack manifest's
    /// `[agent]` harness. Omit to resolve through the configured defaults.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Registered inference provider id the run's agent talks to (see GET /api/config/providers),\nreplacing the pack manifest's `[agent]` harness. Omit to resolve through the configured\ndefaults."
    )]
    pub provider: Option<String>,
    /// The model to ask that provider for. Needs --provider; omit for its default.
    #[arg(long, requires = "provider")]
    #[serde(default)]
    #[schemars(
        description = "The model to ask that provider for, replacing the manifest's `[agent].model`. Needs\n`provider`; omit for the provider's default."
    )]
    pub model: Option<String>,
    /// Rerun this finished run of the same pack instead of starting afresh: every task it
    /// passed is reused, and the tasks --from names run again with everything after them.
    #[arg(long, value_name = "RUN")]
    #[serde(skip)]
    pub rerun_of: Option<String>,
    /// A task the rerun dispatches again, with everything downstream of it. Repeatable.
    #[arg(long = "from", value_name = "TASK", requires = "rerun_of")]
    #[serde(skip)]
    pub from: Vec<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Launch {
    type Output = dto::LaunchAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut body = serde_json::json!({
            "params": self.params,
            "max_cost": self.max_cost,
            "max_time": self.max_time,
        });
        put_trimmed(
            &mut body,
            "dispatch_target",
            self.dispatch_target.as_deref(),
        );
        put_trimmed(&mut body, "provider", self.provider.as_deref());
        put_trimmed(&mut body, "model", self.model.as_deref());
        put_rerun(&mut body, self.rerun_of.as_deref(), &self.from);
        Ok(with_tracker_url(c.launch(&self.id, &body).await?))
    }

    fn render(&self, c: &Client, ack: Self::Output) -> Result<String> {
        let url = c.web_url(&format!("/playbook-runs/{}", encode(&ack.key)));
        Ok(render::launched(&ack, &url))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct PlaybookRuns {
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Status, e.g. `running`, `done`, `parked`. Omit for all.")]
    pub status: Option<String>,
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Only launches of this playbook id.")]
    pub playbook: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for PlaybookRuns {
    type Output = Vec<dto::PlaybookRun>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.playbook_runs(self.status.as_deref(), self.playbook.as_deref())
            .await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::playbook_runs(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Stop a playbook launch: the controller parks it, deletes its running runs' pods on the
/// clusters they run on, and marks those runs cancelled.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Cancel {
    /// Issue key the launch became.
    #[schemars(description = "The launch's issue key, as playbook-runs lists it.")]
    pub key: String,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Cancel {
    type Output = crate::dto::CancelAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        Ok(serde_json::to_value(c.cancel_launch(&self.key).await?)?)
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(crate::render::ack(
            "cancel",
            &ack.key,
            ack.actor.as_deref(),
            &format!("reason: {}", ack.reason),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// One launch, whole. Always JSON: its `params` are arbitrary and the refusals are long, and
/// both are exactly what a caller reads this for.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct PlaybookRun {
    /// Issue key the launch became.
    #[schemars(description = "Issue key.")]
    pub key: String,
    #[arg(skip)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for PlaybookRun {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut out: Value = c.playbook_run(&self.key).await?;
        if let Some(launch) = out.get("launch").cloned() {
            let launch = with_tracker_url(launch);
            if let (Some(url), Some(obj)) = (launch.get("tracker_url"), out.as_object_mut()) {
                obj.insert("tracker_url".to_string(), url.clone());
            }
        }
        Ok(out)
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

/// Propose an import and hand back the link a human opens on it.
///
/// `id` and `description` are the registry naming the proposer suggests; they ride the preview
/// URL as query values so the review page opens on them instead of asking the admin to retype
/// what the proposal already decided. Registering is still the admin's click.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct PlaybookImport {
    /// owner/repo or a clone URL.
    #[schemars(description = "`owner/repo` or a clone URL.")]
    pub repo: String,
    /// Branch or tag. Omit for the repo's default branch.
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Branch or tag to fetch. Omit for the repo's default branch.")]
    pub git_ref: Option<String>,
    /// The pack directory inside the repo. Omit for the repo root.
    #[arg(long)]
    #[serde(default)]
    pub path: Option<String>,
    /// The `[variant.<name>]` of the pack's manifest to preview and register. Omit for the
    /// base manifest.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "The `[variant.<name>]` of the pack's manifest to preview and register. Omit for the base\nmanifest."
    )]
    pub variant: Option<String>,
    /// The registry id to propose registering under; prefills the review page.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "The registry id you propose registering this pack under. Prefills the review page."
    )]
    pub id: Option<String>,
    /// The description to propose; prefills the review page.
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "The description you propose. Prefills the review page.")]
    pub description: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl PlaybookImport {
    fn preview_url(&self, c: &Client, import_id: &str) -> String {
        c.web_url(&import_preview_path(
            import_id,
            self.id.as_deref(),
            self.description.as_deref(),
        ))
    }
}

impl Operation for PlaybookImport {
    type Output = dto::PackImport;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut raw: Value = c
            .import_pack(&PackImportSource {
                repo: &self.repo,
                git_ref: self.git_ref.as_deref(),
                path: self.path.as_deref(),
                variant: self.variant.as_deref(),
            })
            .await?;
        let import: dto::PackImport = serde_json::from_value(raw.clone())?;
        // The URL is this client's fact, not the controller's, and it is the whole point of the
        // call, so it is added rather than left out of the raw payload.
        if let Some(obj) = raw.as_object_mut() {
            obj.insert(
                "preview_url".to_string(),
                Value::String(self.preview_url(c, &import.id)),
            );
        }
        Ok(raw)
    }

    fn render(&self, c: &Client, import: Self::Output) -> Result<String> {
        Ok(render::pack_import(
            &import,
            &self.preview_url(c, &import.id),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

fn import_preview_path(import_id: &str, id: Option<&str>, description: Option<&str>) -> String {
    let query: Vec<String> = [("id", id), ("description", description)]
        .into_iter()
        .filter_map(|(name, value)| {
            let v = value.map(str::trim).filter(|v| !v.is_empty())?;
            Some(format!("{name}={}", encode(v)))
        })
        .collect();
    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    format!("/playbooks/import/{}{suffix}", encode(import_id))
}

#[cfg(test)]
mod tests {
    use crate::ops::playbooks::*;
    use crate::ops::test_client;

    #[test]
    fn a_launch_carries_the_link_that_opens_it_in_the_tracker() {
        let ack = with_tracker_url(serde_json::json!({
            "key": "playbook:triage:01a1", "playbook": "triage", "max_cost": 3.0
        }));
        assert_eq!(
            ack["tracker_url"],
            "crucible-playbooks://playbook/triage/launch/playbook%3Atriage%3A01a1"
        );
        assert_eq!(ack["max_cost"], 3.0, "the ack is otherwise untouched");
        let no_playbook = with_tracker_url(serde_json::json!({"key": "pb-9", "playbook": ""}));
        assert!(no_playbook.get("tracker_url").is_none());
        assert!(
            with_tracker_url(serde_json::json!([1]))
                .get("tracker_url")
                .is_none()
        );
    }

    /// The proposer's naming rides the link so the admin opens the review page on what the agent
    /// decided, rather than retyping it. An id it never supplied must not appear as an empty one.
    #[test]
    fn a_preview_link_carries_the_proposed_naming_when_there_is_any() {
        assert_eq!(
            import_preview_path("0192f4a1", None, None),
            "/playbooks/import/0192f4a1"
        );
        assert_eq!(
            import_preview_path("0192f4a1", Some("  "), Some("")),
            "/playbooks/import/0192f4a1"
        );
        assert_eq!(
            import_preview_path(
                "0192f4a1",
                Some("calibrate"),
                Some("EPP calibration & scoring")
            ),
            "/playbooks/import/0192f4a1?id=calibrate&description=EPP%20calibration%20%26%20scoring"
        );
    }

    /// A registered launch sends the pin the way a test-fire does: trimmed, and omitted rather
    /// than sent blank so the controller's own default applies instead of a 422.
    #[tokio::test]
    async fn a_launch_carries_its_pin_and_omits_what_was_left_blank() {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use std::sync::Arc;
        use std::sync::Mutex;

        type Launched = Arc<Mutex<Vec<Value>>>;

        async fn accept(State(seen): State<Launched>, body: String) -> (StatusCode, String) {
            let sent: Value = serde_json::from_str(&body).expect("a JSON body");
            let ack = serde_json::json!({
                "key": "playbook:survey:1",
                "playbook": "survey",
                "max_cost": sent["max_cost"],
                "max_time": sent["max_time"],
                "actor": "wren",
                "dispatch_target": sent.get("dispatch_target").cloned(),
                "provider": sent.get("provider").cloned(),
                "model": sent.get("model").cloned(),
            });
            seen.lock().expect("lock").push(sent);
            (StatusCode::CREATED, ack.to_string())
        }

        let seen: Launched = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/playbooks/{id}/launch", post(accept))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);
        let bare = Launch {
            id: "survey".into(),
            params: serde_json::json!({}),
            max_cost: 1.0,
            max_time: "5m".into(),
            dispatch_target: None,
            provider: None,
            model: None,
            rerun_of: None,
            from: Vec::new(),
            json: false,
        };

        let pinned = Launch {
            params: serde_json::json!({"topic": "attention sinks"}),
            max_cost: 4.0,
            max_time: "30m".into(),
            dispatch_target: Some("  ".into()),
            provider: Some("plat-openai".into()),
            model: Some(" gpt-5.6-luna ".into()),
            rerun_of: Some("run-7".into()),
            from: vec!["check".into()],
            ..bare.clone()
        }
        .run(&client)
        .await
        .expect("launch");
        assert!(
            pinned.contains("launched survey as playbook:survey:1"),
            "{pinned}"
        );
        assert!(
            pinned.contains("agent plat-openai · gpt-5.6-luna"),
            "{pinned}"
        );
        assert!(
            pinned.contains(&format!("{url}/playbook-runs/playbook%3Asurvey%3A1")),
            "{pinned}"
        );
        let sent = seen.lock().expect("lock")[0].clone();
        assert_eq!(
            sent["params"]["topic"],
            serde_json::json!("attention sinks")
        );
        assert_eq!(sent["max_cost"], serde_json::json!(4.0));
        assert_eq!(sent["max_time"], serde_json::json!("30m"));
        assert_eq!(sent["provider"], serde_json::json!("plat-openai"));
        assert_eq!(sent["model"], serde_json::json!("gpt-5.6-luna"));
        assert!(
            sent.get("dispatch_target").is_none(),
            "a blank target is omitted, not sent: {sent}"
        );
        assert_eq!(
            sent["rerun"],
            serde_json::json!({"of": "run-7", "from": ["check"]})
        );

        let default_model = Launch {
            provider: Some("plat-openai".into()),
            ..bare.clone()
        }
        .run(&client)
        .await
        .expect("launch");
        assert!(
            default_model.contains("agent plat-openai (its default model)"),
            "{default_model}"
        );
        let sent = seen.lock().expect("lock")[1].clone();
        assert_eq!(sent["provider"], serde_json::json!("plat-openai"));
        assert!(sent.get("model").is_none(), "{sent}");

        let unpinned = Launch { json: true, ..bare }
            .run(&client)
            .await
            .expect("launch");
        let ack: Value = serde_json::from_str(&unpinned).expect("json output");
        assert!(ack["provider"].is_null(), "{ack}");
        let sent = seen.lock().expect("lock")[2].clone();
        assert!(
            sent.get("provider").is_none() && sent.get("model").is_none(),
            "{sent}"
        );
    }
}

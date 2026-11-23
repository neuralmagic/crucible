//! Schedules, secrets, shares and decision requests, read and changed through crux on a task of
//! their own so a slow call never holds up the launch list.

mod act;
pub mod app;
pub mod client;
pub mod desk;
pub mod forms;

use std::collections::BTreeMap;

use crux::client::Shared;
use crux::dto;

/// A read that has not come back yet, failed, or landed.
pub type Loaded<T> = Option<Result<T, String>>;

/// What the window shows, and so what the worker keeps current.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Watch {
    /// The controller backend is on; nothing is read while it is not.
    pub controller: bool,
    pub schedules: bool,
    pub secrets: bool,
    pub secret: Option<String>,
    pub shares: Option<Shared>,
    pub decision: Option<String>,
    /// The API keys page is shown.
    pub keys: bool,
    pub teams: bool,
    /// The team whose audit trail the Teams page shows.
    pub team_audit: Option<String>,
    pub webhooks: bool,
    /// The webhook whose deliveries the Triggers page shows.
    pub webhook: Option<String>,
    /// The playbook the open webhook form launches.
    pub webhook_form: Option<String>,
    pub providers: bool,
    pub blueprints: bool,
    pub activity: bool,
    /// The Admin page is shown.
    pub admin: bool,
    /// The Admin page's policy tab is shown.
    pub policy: bool,
    /// The Settings page is shown, with the controller's version on it.
    pub about: bool,
    /// The Issues page is shown, listing this status or every one.
    pub issues: Option<InStatus>,
    /// The issue the Issues page shows in full.
    pub issue: Option<String>,
}

/// A change to make on the controller.
#[derive(Clone, Debug)]
pub enum Action {
    Enable {
        schedule: Box<dto::Schedule>,
        on: bool,
    },
    Grant {
        of: Shared,
        grantee: String,
        role: String,
        not_after: Option<String>,
    },
    Revoke {
        of: Shared,
        grantee: String,
    },
    Rotate {
        secret: String,
        value: String,
    },
    Unbind {
        secret: String,
        binding: String,
    },
    Answer {
        id: String,
        labels: BTreeMap<String, Vec<String>>,
        digest: String,
        note: Option<String>,
    },
    CreateSecret {
        body: serde_json::Value,
    },
    MoveSecret {
        secret: String,
        owner: String,
    },
    /// Fire schedule `id` now instead of at its next window.
    FireSchedule {
        id: String,
        playbook: String,
    },
    /// Launch schedule `id` once with `cursor`, leaving the schedule as it is.
    TestFireSchedule {
        id: String,
        playbook: String,
        cursor: dto::TestCursor,
    },
    /// Create a schedule, or replace schedule `id` whole.
    SaveSchedule {
        id: Option<String>,
        body: serde_json::Value,
    },
    /// Register pending import `import` as registry id `id`.
    RegisterImport {
        import: String,
        id: String,
        description: String,
    },
    DiscardImport {
        import: String,
    },
    /// Stop playbook launch `key`: park it and delete its running pods.
    CancelLaunch {
        key: String,
    },
    /// Mint an API key that expires in `days`, or never.
    MintKey {
        name: String,
        days: Option<i64>,
    },
    RevokeKey {
        id: String,
    },
    /// Sign out one MCP client, or all of them.
    SignOutMcp {
        client: Option<String>,
    },
    CreateTeam {
        slug: String,
        name: String,
    },
    RenameTeam {
        slug: String,
        name: String,
    },
    DeleteTeam {
        slug: String,
    },
    /// Replace team `slug`'s whole member list.
    PutMembers {
        slug: String,
        members: Vec<dto::TeamMemberBody>,
    },
    /// Resume (`on`) or pause a webhook.
    EnableWebhook {
        id: String,
        on: bool,
    },
    RotateWebhook {
        id: String,
    },
    DeleteWebhook {
        id: String,
    },
    /// Create a webhook, or replace webhook `id`.
    SaveWebhook {
        id: Option<String>,
        body: Box<dto::WebhookBody>,
    },
    CheckWebhook {
        body: Box<dto::WebhookBody>,
    },
    /// Register a provider, or replace registration `id`.
    SaveProvider {
        id: Option<String>,
        register: Box<dto::RegisterProvider>,
    },
    DeleteProvider {
        id: String,
    },
    /// Store a policy set. It is not activated.
    CreatePolicy {
        text: String,
    },
    ActivatePolicy {
        digest: String,
    },
    /// Ask how the active policy decides `action` for `login`.
    Explain {
        login: String,
        action: String,
        resource: Option<String>,
    },
    /// Replace the controller's whole override set.
    SetOverrides {
        overrides: serde_json::Map<String, serde_json::Value>,
        justification: String,
    },
    SetAutopilot {
        enabled: bool,
        reason: String,
    },
    Reconcile,
    AddRepo {
        repo: String,
        justification: String,
    },
    /// Pause discovery for a repo, or resume it.
    PauseRepo {
        repo: String,
        paused: bool,
    },
    UnwatchRepo {
        repo: String,
    },
    /// Start draft `to` from blueprint `id`.
    UseBlueprint {
        id: String,
        to: String,
        description: Option<String>,
    },
    /// Run the form's transform on a sample delivery.
    PreviewWebhook {
        body: Box<dto::WebhookBody>,
        sample: serde_json::Value,
        headers: BTreeMap<String, String>,
    },
    ParkIssue {
        key: String,
        reason: String,
    },
    UnparkIssue {
        key: String,
    },
}

/// Which kind of action an outcome answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Did {
    Enable,
    Grant,
    Revoke,
    Rotate,
    Unbind,
    Answer,
    CreateSecret,
    MoveSecret,
    FireSchedule,
    TestFireSchedule,
    SaveSchedule,
    RegisterImport,
    DiscardImport,
    CancelLaunch,
    MintKey,
    RevokeKey,
    SignOutMcp,
    CreateTeam,
    RenameTeam,
    DeleteTeam,
    PutMembers,
    EnableWebhook,
    RotateWebhook,
    DeleteWebhook,
    SaveWebhook,
    CheckWebhook,
    PreviewWebhook,
    SaveProvider,
    DeleteProvider,
    UseBlueprint,
    SetOverrides,
    SetAutopilot,
    Reconcile,
    AddRepo,
    PauseRepo,
    UnwatchRepo,
    CreatePolicy,
    ActivatePolicy,
    Explain,
    ParkIssue,
    UnparkIssue,
}

impl Action {
    fn did(&self) -> Did {
        match self {
            Action::Enable { .. } => Did::Enable,
            Action::Grant { .. } => Did::Grant,
            Action::Revoke { .. } => Did::Revoke,
            Action::Rotate { .. } => Did::Rotate,
            Action::Unbind { .. } => Did::Unbind,
            Action::Answer { .. } => Did::Answer,
            Action::CreateSecret { .. } => Did::CreateSecret,
            Action::MoveSecret { .. } => Did::MoveSecret,
            Action::FireSchedule { .. } => Did::FireSchedule,
            Action::TestFireSchedule { .. } => Did::TestFireSchedule,
            Action::SaveSchedule { .. } => Did::SaveSchedule,
            Action::RegisterImport { .. } => Did::RegisterImport,
            Action::DiscardImport { .. } => Did::DiscardImport,
            Action::CancelLaunch { .. } => Did::CancelLaunch,
            Action::MintKey { .. } => Did::MintKey,
            Action::RevokeKey { .. } => Did::RevokeKey,
            Action::SignOutMcp { .. } => Did::SignOutMcp,
            Action::CreateTeam { .. } => Did::CreateTeam,
            Action::RenameTeam { .. } => Did::RenameTeam,
            Action::DeleteTeam { .. } => Did::DeleteTeam,
            Action::PutMembers { .. } => Did::PutMembers,
            Action::EnableWebhook { .. } => Did::EnableWebhook,
            Action::RotateWebhook { .. } => Did::RotateWebhook,
            Action::DeleteWebhook { .. } => Did::DeleteWebhook,
            Action::SaveWebhook { .. } => Did::SaveWebhook,
            Action::CheckWebhook { .. } => Did::CheckWebhook,
            Action::PreviewWebhook { .. } => Did::PreviewWebhook,
            Action::SaveProvider { .. } => Did::SaveProvider,
            Action::DeleteProvider { .. } => Did::DeleteProvider,
            Action::UseBlueprint { .. } => Did::UseBlueprint,
            Action::SetOverrides { .. } => Did::SetOverrides,
            Action::SetAutopilot { .. } => Did::SetAutopilot,
            Action::Reconcile => Did::Reconcile,
            Action::AddRepo { .. } => Did::AddRepo,
            Action::PauseRepo { .. } => Did::PauseRepo,
            Action::UnwatchRepo { .. } => Did::UnwatchRepo,
            Action::CreatePolicy { .. } => Did::CreatePolicy,
            Action::ActivatePolicy { .. } => Did::ActivatePolicy,
            Action::Explain { .. } => Did::Explain,
            Action::ParkIssue { .. } => Did::ParkIssue,
            Action::UnparkIssue { .. } => Did::UnparkIssue,
        }
    }

    fn what(&self) -> String {
        match self {
            Action::Enable { schedule, on: true } => format!("Enabled {}", schedule.playbook),
            Action::Enable { schedule, .. } => format!("Disabled {}", schedule.playbook),
            Action::Grant { grantee, role, .. } => format!("Shared with {grantee} as {role}"),
            Action::Revoke { grantee, .. } => format!("Stopped sharing with {grantee}"),
            Action::Rotate { .. } => "Rotated".to_string(),
            Action::Unbind { .. } => "Unbound".to_string(),
            Action::Answer { .. } => "Answered".to_string(),
            Action::CreateSecret { body } => format!(
                "Created {}",
                body.get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("the secret")
            ),
            Action::MoveSecret { owner, .. } => format!("Moved to {owner}"),
            Action::FireSchedule { playbook, .. } => format!("Firing {playbook}"),
            Action::TestFireSchedule { playbook, .. } => format!("Test-firing {playbook}"),
            Action::SaveSchedule { id: None, .. } => "Scheduled".to_string(),
            Action::SaveSchedule { id: Some(_), .. } => "Saved the schedule".to_string(),
            Action::RegisterImport { id, .. } => format!("Registered {id}"),
            Action::DiscardImport { .. } => "Discarded the import".to_string(),
            Action::CancelLaunch { .. } => "Cancelling the launch".to_string(),
            Action::MintKey { name, .. } => format!("Minted {name}"),
            Action::RevokeKey { .. } => "Revoked".to_string(),
            Action::SignOutMcp { .. } => "Signed out".to_string(),
            Action::CreateTeam { slug, .. } => format!("Created {slug}"),
            Action::RenameTeam { name, .. } => format!("Renamed to {name}"),
            Action::DeleteTeam { slug } => format!("Deleted {slug}"),
            Action::PutMembers { .. } => "Saved the members".to_string(),
            Action::EnableWebhook { on: true, .. } => "Resumed the webhook".to_string(),
            Action::EnableWebhook { .. } => "Paused the webhook".to_string(),
            Action::RotateWebhook { .. } => "Rotated the secret".to_string(),
            Action::DeleteWebhook { .. } => "Deleted the webhook".to_string(),
            Action::SaveWebhook { id: None, .. } => "Created the webhook".to_string(),
            Action::SaveWebhook { id: Some(_), .. } => "Saved the webhook".to_string(),
            Action::CheckWebhook { .. } => "Checked".to_string(),
            Action::PreviewWebhook { .. } => "Previewed".to_string(),
            Action::SaveProvider { id: None, register } => format!("Registered {}", register.id),
            Action::SaveProvider { id: Some(_), .. } => "Saved the provider".to_string(),
            Action::DeleteProvider { id } => format!("Deregistered {id}"),
            Action::UseBlueprint { to, .. } => format!("Started draft {to}"),
            Action::SetOverrides { .. } => "Saved the overrides".to_string(),
            Action::SetAutopilot { enabled: true, .. } => "Autopilot on".to_string(),
            Action::SetAutopilot { .. } => "Autopilot off".to_string(),
            Action::Reconcile => "Reconcile asked for".to_string(),
            Action::AddRepo { repo, .. } => format!("Watching {repo}"),
            Action::PauseRepo { repo, paused: true } => format!("Paused {repo}"),
            Action::PauseRepo { repo, .. } => format!("Resumed {repo}"),
            Action::UnwatchRepo { repo } => format!("Stopped watching {repo}"),
            Action::CreatePolicy { .. } => "Stored the policy set".to_string(),
            Action::ActivatePolicy { .. } => "Activated the policy set".to_string(),
            Action::Explain { .. } => "Explained".to_string(),
            Action::ParkIssue { key, .. } => format!("Parked {key}"),
            Action::UnparkIssue { key } => format!("Unparked {key}"),
        }
    }
}

/// What the last action came to. `seq` only grows.
#[derive(Clone, Debug, PartialEq)]
pub struct Acted {
    pub seq: u64,
    pub did: Did,
    pub result: Result<String, String>,
}

/// A secret with its bindings and audit trail.
#[derive(Clone, Debug)]
pub struct SecretView {
    pub detail: dto::SecretDetail,
    pub audit: Vec<dto::SecretAudit>,
}

/// What the worker last read, for the window.
#[derive(Clone, Default)]
pub struct AdminView {
    pub schedules: Loaded<Vec<dto::Schedule>>,
    pub secrets: Loaded<Vec<dto::Secret>>,
    pub secret: Option<(String, Loaded<SecretView>)>,
    pub shares: Option<(Shared, Loaded<Vec<dto::Share>>)>,
    pub decisions: Loaded<Vec<dto::DecisionSummary>>,
    pub decision: Option<(String, Loaded<dto::Decision>)>,
    /// The open decision requests read in full, by id, for the notes that announce them.
    pub details: BTreeMap<String, dto::Decision>,
    /// Scope packs to approve, imports to settle and kept PRs to review.
    pub approvals: Loaded<dto::Approvals>,
    pub keys: Loaded<Vec<dto::ApiKey>>,
    pub mcp: Loaded<dto::McpSignIns>,
    pub teams: Loaded<Vec<dto::Team>>,
    pub team_audit: Option<(String, Loaded<Vec<dto::TeamAudit>>)>,
    pub webhooks: Loaded<Vec<dto::Webhook>>,
    pub deliveries: Option<(String, Loaded<Vec<dto::WebhookDelivery>>)>,
    /// The secret of the webhook just created or rotated, until another webhook is opened or
    /// the Triggers page is left.
    pub rotated: Option<dto::WebhookSecret>,
    /// The open webhook form's playbook and its params schema.
    pub webhook_schema: Option<(String, Loaded<serde_json::Value>)>,
    pub presets: Loaded<Vec<dto::WebhookPreset>>,
    pub providers: Loaded<Vec<dto::Provider>>,
    pub blueprints: Loaded<Vec<dto::Blueprint>>,
    /// The build the controller is running.
    pub version: Loaded<dto::Version>,
    /// The issues in the status they were asked for.
    pub issues: Option<(InStatus, Loaded<Vec<dto::Issue>>)>,
    pub issue: Option<(String, Loaded<dto::IssueDetail>)>,
    /// The draft the last blueprint use started, while the Blueprints page stays open.
    pub used: Option<dto::BlueprintUsed>,
    pub events: Loaded<Vec<dto::Event>>,
    pub config: Loaded<dto::ControllerConfig>,
    pub access: Loaded<dto::Access>,
    pub autopilot: Loaded<dto::Autopilot>,
    pub repos: Loaded<Vec<dto::WatchedRepo>>,
    pub policy_sets: Loaded<Vec<dto::PolicySet>>,
    /// The set just stored, to show it once the list has it.
    pub stored_policy: Option<String>,
    pub explained: Option<dto::Explanation>,
    /// What the last check and preview of the open webhook form came to.
    pub webhook_check: Option<dto::WebhookCheck>,
    pub webhook_preview: Option<dto::WebhookPreview>,
    /// The key just minted, with its secret, for as long as the API keys page stays open.
    pub minted: Option<dto::MintedKey>,
    pub acted: Option<Acted>,
}

/// The status an issue list is asked for; none is every status.
pub type InStatus = Option<String>;

/// `in 3h`, `in 12m` or `5d ago` from Unix seconds `now` to the RFC 3339 `stamp`; empty when it
/// does not parse.
pub fn relative(stamp: &str, now: i64) -> String {
    let Ok(at) = stamp.parse::<jiff::Timestamp>() else {
        return String::new();
    };
    let delta = at.as_second() - now;
    let span = delta.unsigned_abs();
    let amount = match span {
        0..60 => return "now".to_string(),
        60..3_600 => format!("{}m", span / 60),
        3_600..86_400 => format!("{}h", span / 3_600),
        _ => format!("{}d", span / 86_400),
    };
    if delta > 0 {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

#[cfg(test)]
mod tests {
    use crate::admin::app::fixture;
    use crate::admin::client::Client;
    use crate::admin::desk::Desk;
    use crate::admin::*;
    use rune::TextEdit;
    use rune::time::Instant;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::admin::forms::*;

    #[test]
    fn the_decision_fixture_reads_as_an_open_answerable_request() {
        let decision = fixture().expect("fixture");
        assert_eq!(decision.state, "open");
        assert!(decision.can_answer);
        assert_eq!(decision.questions.len(), 2);
        let mut desk = Desk::default();
        assert_eq!(desk.unanswered(&decision), ["ship", "repos"]);
        desk.toggle_pick("ship", "yes", false);
        desk.toggle_pick("repos", "llm-d/llm-d", true);
        assert!(desk.unanswered(&decision).is_empty());
    }

    #[test]
    fn everything_watched_is_read_at_once() {
        let slow = Duration::from_secs(1);
        let (url, _) = crate::testserver::serve(move |request| {
            std::thread::sleep(slow);
            match request.split_whitespace().nth(1) {
                Some("/api/decisions") => r#"{"open":[]}"#.into(),
                Some("/api/approvals") => "{}".into(),
                _ => "[]".into(),
            }
        });
        let keys = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client =
            Client::spawn(net.connector(crate::testserver::target("t", &url, keys.path())));
        let asked = Instant::now();
        client.watch(Watch {
            controller: true,
            schedules: true,
            secrets: true,
            ..Watch::default()
        });
        let view = loop {
            if let Some((_, view)) = client.take(0) {
                break view;
            }
            assert!(
                asked.elapsed() < Duration::from_secs(10),
                "nothing was read"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            asked.elapsed() < slow * 3,
            "four reads of {slow:?} each took {:?}",
            asked.elapsed()
        );
        assert_eq!(view.decisions, Some(Ok(Vec::new())));
        assert_eq!(view.approvals.map(|a| a.map(|a| a.waiting())), Some(Ok(0)));
        assert_eq!(view.schedules.map(|s| s.map(|s| s.len())), Some(Ok(0)));
        assert_eq!(view.secrets.map(|s| s.map(|s| s.len())), Some(Ok(0)));
        assert!(view.secret.is_none() && view.shares.is_none() && view.decision.is_none());
    }

    /// A worker reading a server that answers each `[method, path]` with `answer`, and every
    /// request line that server took.
    fn worker(
        answer: impl Fn(&[&str]) -> String + Send + Sync + 'static,
    ) -> (Client, Arc<Mutex<Vec<String>>>, tempfile::TempDir) {
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&requests);
        let (url, _) = crate::testserver::serve(move |request| {
            let line: Vec<&str> = request.split_whitespace().take(2).collect();
            seen.lock().unwrap().push(line.join(" "));
            answer(&line)
        });
        let keys = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client =
            Client::spawn(net.connector(crate::testserver::target("t", &url, keys.path())));
        (client, requests, keys)
    }

    /// The newest view once `done` holds for it.
    fn until(client: &Client, done: impl Fn(&AdminView) -> bool) -> AdminView {
        let asked = Instant::now();
        loop {
            if let Some((_, view)) = client.take(0)
                && done(&view)
            {
                return view;
            }
            assert!(
                asked.elapsed() < Duration::from_secs(10),
                "the view never got there"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn keys_are_read_only_while_their_page_shows_and_a_minted_secret_lasts_as_long() {
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/keys"] => r#"[{"id":"k1","name":"laptop",
                    "created_at":"2026-11-01T00:00:00Z","expires_at":null,
                    "last_used_at":null,"revoked_at":null}]"#
                .into(),
            ["POST", "/api/keys"] => r#"{"id":"k2","name":"ci",
                    "created_at":"2026-11-10T00:00:00Z","expires_at":null,"last_used_at":null,
                    "revoked_at":null,"secret":"crk_once","mcp_url":null}"#
                .into(),
            ["GET", "/api/me/mcp-sign-ins"] => r#"{"enabled":true,"sign_ins":[]}"#.into(),
            ["DELETE", "/api/me/mcp-sign-ins?client_id=dyn-1"] => r#"{"revoked":1}"#.into(),
            _ => "[]".into(),
        });
        let elsewhere = Watch {
            controller: true,
            ..Watch::default()
        };
        let on_page = Watch {
            keys: true,
            ..elsewhere.clone()
        };

        client.watch(elsewhere.clone());
        let view = until(&client, |view| view.decisions.is_some());
        assert!(view.keys.is_none() && view.mcp.is_none());
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("/api/keys")),
            "keys were read off their page"
        );

        client.watch(on_page);
        let view = until(&client, |view| view.keys.is_some() && view.mcp.is_some());
        assert_eq!(view.keys.map(|k| k.map(|k| k.len())), Some(Ok(1)));
        assert_eq!(view.mcp.map(|m| m.map(|m| m.enabled)), Some(Ok(true)));

        client.act(Action::MintKey {
            name: "ci".into(),
            days: None,
        });
        let view = until(&client, |view| view.acted.is_some());
        assert_eq!(
            view.acted.map(|a| (a.did, a.result)),
            Some((Did::MintKey, Ok("Minted ci".to_string())))
        );
        assert_eq!(view.minted.map(|m| m.secret), Some("crk_once".to_string()));

        client.act(Action::RevokeKey { id: "k1".into() });
        until(&client, |view| {
            view.acted.as_ref().is_some_and(|a| a.did == Did::RevokeKey)
        });
        client.act(Action::SignOutMcp {
            client: Some("dyn-1".into()),
        });
        let view = until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::SignOutMcp)
        });
        assert_eq!(
            view.acted.map(|a| a.result),
            Some(Ok("Signed out".to_string()))
        );
        assert!(
            view.minted.is_some(),
            "the secret went while its page showed"
        );
        let sent = requests.lock().unwrap().clone();
        for change in [
            "POST /api/keys",
            "DELETE /api/keys/k1",
            "DELETE /api/me/mcp-sign-ins?client_id=dyn-1",
        ] {
            assert!(sent.iter().any(|r| r == change), "{change} was not sent");
        }

        client.watch(elsewhere);
        until(&client, |view| view.minted.is_none());
    }

    #[test]
    fn teams_are_read_on_their_page_with_the_audit_of_the_one_asked_for() {
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/teams"] => r#"[{"slug":"infra","display_name":"Infra",
                    "members":[{"kind":"user","member":"wynn","role":"owner"}],
                    "my_role":"owner","reachable":true}]"#
                .into(),
            ["GET", "/api/teams/infra/audit"] => r#"[{"id":7,"at":"2026-11-01T00:00:00Z",
                    "actor":"user:wynn","action":"team:create","decision":"allow","rule":"owner"}]"#
                .into(),
            ["POST", "/api/teams"] | ["PUT", _] => {
                r#"{"slug":"infra","display_name":"Infra"}"#.into()
            }
            _ => "[]".into(),
        });
        let listed = Watch {
            controller: true,
            teams: true,
            ..Watch::default()
        };

        client.watch(listed.clone());
        let view = until(&client, |view| view.teams.is_some());
        assert_eq!(
            view.teams
                .map(|teams| teams.map(|teams| (teams.len(), teams[0].my_role))),
            Some(Ok((1, Some(dto::TeamRole::Owner))))
        );
        assert!(view.team_audit.is_none());
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.ends_with("/audit")),
            "an audit trail nobody asked for was read"
        );

        client.watch(Watch {
            team_audit: Some("infra".into()),
            ..listed.clone()
        });
        let view = until(&client, |view| view.team_audit.is_some());
        let (slug, audit) = view.team_audit.unwrap();
        assert_eq!(slug, "infra");
        assert_eq!(
            audit.map(|a| a.map(|a| a[0].action.clone())),
            Some(Ok("team:create".to_string()))
        );

        let steps = [
            (
                Action::CreateTeam {
                    slug: "infra".into(),
                    name: "Infra".into(),
                },
                Did::CreateTeam,
                "Created infra",
                "POST /api/teams",
            ),
            (
                Action::RenameTeam {
                    slug: "infra".into(),
                    name: "Platform".into(),
                },
                Did::RenameTeam,
                "Renamed to Platform",
                "PUT /api/teams/infra",
            ),
            (
                Action::PutMembers {
                    slug: "infra".into(),
                    members: Vec::new(),
                },
                Did::PutMembers,
                "Saved the members",
                "PUT /api/teams/infra/members",
            ),
            (
                Action::DeleteTeam {
                    slug: "infra".into(),
                },
                Did::DeleteTeam,
                "Deleted infra",
                "DELETE /api/teams/infra",
            ),
        ];
        for (action, did, said, sent) in steps {
            client.act(action);
            let view = until(&client, |view| {
                view.acted.as_ref().is_some_and(|a| a.did == did)
            });
            assert_eq!(view.acted.map(|a| a.result), Some(Ok(said.to_string())));
            assert!(
                requests.lock().unwrap().iter().any(|r| r == sent),
                "{sent} was not sent"
            );
        }

        client.watch(listed);
        until(&client, |view| view.team_audit.is_none());
    }

    #[test]
    fn webhooks_are_read_on_triggers_and_a_rotated_secret_lasts_while_its_webhook_shows() {
        const HOOK: &str = r#"{"id":"w1","playbook":"triage","verifier":"path_token",
            "enabled":true,"max_launches_per_hour":3}"#;
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/webhooks"] => format!("[{HOOK}]"),
            ["GET", "/api/webhooks/w1/deliveries?limit=20"] => r#"[{"id":"d1",
                    "received_at":"2026-11-01T00:00:00Z","outcome":"filtered",
                    "reason":"the filter was false","launch_key":null}]"#
                .into(),
            ["POST", "/api/webhooks/w1/secret"] => {
                format!(r#"{{"webhook":{HOOK},"secret":"whsec_once","delivery_url":null}}"#)
            }
            ["POST", "/api/webhooks/w1/enabled"] => HOOK.into(),
            _ => "[]".into(),
        });
        let listed = Watch {
            controller: true,
            schedules: true,
            webhooks: true,
            ..Watch::default()
        };
        let opened = Watch {
            webhook: Some("w1".into()),
            ..listed.clone()
        };

        client.watch(listed.clone());
        let view = until(&client, |view| view.webhooks.is_some());
        assert_eq!(
            view.webhooks
                .map(|hooks| hooks.map(|hooks| hooks[0].playbook.clone())),
            Some(Ok("triage".to_string()))
        );
        assert!(view.deliveries.is_none());
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("/deliveries")),
            "deliveries nobody opened were read"
        );

        client.watch(opened);
        let view = until(&client, |view| view.deliveries.is_some());
        let (id, list) = view.deliveries.unwrap();
        assert_eq!(id, "w1");
        assert_eq!(
            list.map(|list| list.map(|list| list[0].outcome.clone())),
            Some(Ok("filtered".to_string()))
        );

        client.act(Action::RotateWebhook { id: "w1".into() });
        let view = until(&client, |view| view.rotated.is_some());
        assert_eq!(
            view.rotated.map(|r| r.secret),
            Some("whsec_once".to_string())
        );
        client.act(Action::EnableWebhook {
            id: "w1".into(),
            on: false,
        });
        let view = until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::EnableWebhook)
        });
        assert_eq!(
            view.acted.map(|a| a.result),
            Some(Ok("Paused the webhook".to_string()))
        );
        assert!(
            view.rotated.is_some(),
            "the secret went while its webhook showed"
        );
        client.act(Action::DeleteWebhook { id: "w1".into() });
        until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::DeleteWebhook)
        });
        let sent = requests.lock().unwrap().clone();
        for change in [
            "POST /api/webhooks/w1/secret",
            "POST /api/webhooks/w1/enabled",
            "DELETE /api/webhooks/w1",
        ] {
            assert!(sent.iter().any(|r| r == change), "{change} was not sent");
        }

        client.watch(listed);
        let view = until(&client, |view| view.deliveries.is_none());
        assert!(
            view.rotated.is_some(),
            "closing the details dropped the secret"
        );
        client.watch(Watch {
            controller: true,
            ..Watch::default()
        });
        until(&client, |view| view.rotated.is_none());
    }

    #[test]
    fn a_webhook_form_reads_its_schema_and_presets_and_a_created_secret_is_kept() {
        const HOOK: &str = r#"{"id":"w2","playbook":"triage","verifier":"path_token"}"#;
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/playbooks/triage/schema"] => {
                r#"{"properties":{"repo":{"type":"string"}},"required":["repo"]}"#.into()
            }
            ["GET", "/api/webhooks/presets"] => r#"[{"id":"quay-push","title":"quay.io push",
                    "verifier":"path_token","header":null,"filter":"true","dedupe":"delivery",
                    "derive":{}}]"#
                .into(),
            ["POST", "/api/webhooks"] => {
                format!(r#"{{"webhook":{HOOK},"secret":"whsec_new","delivery_url":null}}"#)
            }
            ["PUT", "/api/webhooks/w2"] => HOOK.into(),
            ["POST", "/api/webhooks/check"] => r#"{"diagnostics":[]}"#.into(),
            ["POST", "/api/webhooks/preview"] => r#"{"filter":{"value":true},
                    "dedupe":{"value":"d1"},"derive":{"error":"no such key"}}"#
                .into(),
            _ => "[]".into(),
        });
        let listed = Watch {
            controller: true,
            webhooks: true,
            ..Watch::default()
        };
        let filling = Watch {
            webhook_form: Some("triage".into()),
            ..listed.clone()
        };
        let mut form = crate::webhook::WebhookForm::new("triage");

        client.watch(listed.clone());
        let view = until(&client, |view| view.webhooks.is_some());
        assert!(view.webhook_schema.is_none() && view.presets.is_none());

        client.watch(filling);
        let view = until(&client, |view| {
            view.webhook_schema.is_some() && view.presets.is_some()
        });
        let (playbook, schema) = view.webhook_schema.unwrap();
        form.lay_out(&playbook, &crate::form::fields(&schema.unwrap().unwrap()));
        assert_eq!(form.choices[0].name, "repo");
        assert_eq!(view.presets.unwrap().unwrap()[0].id, "quay-push");
        let body = Box::new(form.body().unwrap());

        client.act(Action::CheckWebhook { body: body.clone() });
        let view = until(&client, |view| view.webhook_check.is_some());
        assert!(view.webhook_check.unwrap().diagnostics.is_empty());
        client.act(Action::PreviewWebhook {
            body: body.clone(),
            sample: serde_json::json!({}),
            headers: BTreeMap::new(),
        });
        let view = until(&client, |view| view.webhook_preview.is_some());
        let preview = view.webhook_preview.unwrap();
        assert_eq!(preview.filter.value, Some(serde_json::json!(true)));
        assert_eq!(preview.derive.error.as_deref(), Some("no such key"));

        client.act(Action::SaveWebhook {
            id: None,
            body: body.clone(),
        });
        let view = until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::SaveWebhook)
        });
        assert_eq!(
            view.acted.map(|a| a.result),
            Some(Ok("Created the webhook".to_string()))
        );
        assert_eq!(
            view.rotated.map(|r| r.secret),
            Some("whsec_new".to_string())
        );
        client.act(Action::SaveWebhook {
            id: Some("w2".into()),
            body,
        });
        until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.result == Ok("Saved the webhook".to_string()))
        });
        let sent = requests.lock().unwrap().clone();
        for change in ["POST /api/webhooks", "PUT /api/webhooks/w2"] {
            assert!(sent.iter().any(|r| r == change), "{change} was not sent");
        }

        client.watch(listed);
        let view = until(&client, |view| {
            view.webhook_check.is_none() && view.webhook_preview.is_none()
        });
        assert!(view.webhook_schema.is_none());
        assert_eq!(
            view.rotated.map(|r| r.secret),
            Some("whsec_new".to_string()),
            "closing the form dropped the new webhook's secret"
        );
    }

    #[test]
    fn providers_are_read_on_their_page_and_a_save_registers_or_replaces() {
        const PROVIDER: &str = r#"{"id":"lab","display_name":"Lab","kind":"openai",
            "default_model":"gpt-5","enabled":true,"owner":"user:wynn","actions":["update"]}"#;
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/providers"] => format!("[{PROVIDER}]"),
            ["POST", "/api/providers"] | ["PUT", "/api/providers/lab"] => PROVIDER.into(),
            _ => "[]".into(),
        });
        let elsewhere = Watch {
            controller: true,
            ..Watch::default()
        };

        client.watch(elsewhere.clone());
        let view = until(&client, |view| view.decisions.is_some());
        assert!(view.providers.is_none());
        client.watch(Watch {
            providers: true,
            ..elsewhere
        });
        let view = until(&client, |view| view.providers.is_some());
        let stored = view.providers.unwrap().unwrap().remove(0);
        assert!(crate::provider::may(&stored, "update"));

        let mut form = crate::provider::ProviderForm::editing(&stored);
        form.enabled = false;
        let register = Box::new(form.register().unwrap());
        let steps = [
            (None, "Registered lab", "POST /api/providers"),
            (
                Some("lab".to_string()),
                "Saved the provider",
                "PUT /api/providers/lab",
            ),
        ];
        for (id, said, sent) in steps {
            client.act(Action::SaveProvider {
                id,
                register: register.clone(),
            });
            until(&client, |view| {
                view.acted
                    .as_ref()
                    .is_some_and(|a| a.result == Ok(said.to_string()))
            });
            assert!(
                requests.lock().unwrap().iter().any(|r| r == sent),
                "{sent} was not sent"
            );
        }
        client.act(Action::DeleteProvider { id: "lab".into() });
        until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::DeleteProvider)
        });
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r == "DELETE /api/providers/lab")
        );
    }

    #[test]
    fn the_controllers_version_is_read_while_settings_show_it() {
        let (client, _requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/version"] => {
                r#"{"git_sha":"abc123def456","version":"0.6.1","autoresearch":true}"#.into()
            }
            _ => "[]".into(),
        });
        let elsewhere = Watch {
            controller: true,
            ..Watch::default()
        };
        client.watch(elsewhere.clone());
        let view = until(&client, |view| view.decisions.is_some());
        assert!(view.version.is_none());

        client.watch(Watch {
            about: true,
            ..elsewhere
        });
        let view = until(&client, |view| view.version.is_some());
        let version = view.version.unwrap().unwrap();
        assert_eq!(
            (version.version.as_str(), version.git_sha.as_str()),
            ("0.6.1", "abc123def456")
        );
    }

    #[test]
    fn issues_are_read_in_the_status_shown_and_one_is_parked_and_unparked() {
        const ISSUE: &str = r#"{"key":"github:acme/engine#7","repo":"acme/engine",
            "kind":{"type":"github","owner":"acme","repo":"engine","number":7},
            "status":"running","title":"Slow start","updated_at":"2026-11-10T18:01:39Z"}"#;
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/issues?exclude_kind=playbook&status=running"] => format!("[{ISSUE}]"),
            ["GET", "/api/issues?exclude_kind=playbook"] => format!("[{ISSUE},{ISSUE}]"),
            ["GET", "/api/issues/github%3Aacme%2Fengine%237"] => format!(
                r#"{{"issue":{ISSUE},"body":"It is slow","events":[{{"ts":"2026-11-10T18:00:00Z",
                "key":"github:acme/engine#7","from":"scoped","to":"running"}}]}}"#
            ),
            ["POST", _] => "{}".into(),
            _ => "[]".into(),
        });
        let elsewhere = Watch {
            controller: true,
            ..Watch::default()
        };

        client.watch(elsewhere.clone());
        let view = until(&client, |view| view.decisions.is_some());
        assert!(view.issues.is_none() && view.issue.is_none());

        let running = Some("running".to_string());
        client.watch(Watch {
            issues: Some(running.clone()),
            ..elsewhere.clone()
        });
        let view = until(&client, |view| view.issues.is_some());
        let (status, issues) = view.issues.unwrap();
        assert_eq!(status, running);
        assert_eq!(
            issues.unwrap().unwrap()[0].title.as_deref(),
            Some("Slow start")
        );

        client.watch(Watch {
            issues: Some(None),
            issue: Some("github:acme/engine#7".into()),
            ..elsewhere.clone()
        });
        let view = until(&client, |view| {
            view.issue.is_some() && matches!(&view.issues, Some((None, _)))
        });
        assert_eq!(view.issues.unwrap().1.unwrap().unwrap().len(), 2);
        let (key, detail) = view.issue.unwrap();
        let detail = detail.unwrap().unwrap();
        assert_eq!(key, "github:acme/engine#7");
        assert_eq!(detail.body.as_deref(), Some("It is slow"));
        assert_eq!(detail.events[0].to, "running");

        client.act(Action::ParkIssue {
            key: key.clone(),
            reason: "needs a repro".into(),
        });
        let view = until(&client, |view| view.acted.is_some());
        assert_eq!(
            view.acted.map(|a| a.result),
            Some(Ok("Parked github:acme/engine#7".to_string()))
        );
        client.act(Action::UnparkIssue { key });
        until(&client, |view| {
            view.acted.as_ref().map(|a| &a.result)
                == Some(&Ok("Unparked github:acme/engine#7".to_string()))
        });
        let asked = requests.lock().unwrap();
        for post in [
            "POST /api/issues/github%3Aacme%2Fengine%237/park",
            "POST /api/issues/github%3Aacme%2Fengine%237/unpark",
        ] {
            assert!(asked.iter().any(|r| r == post), "{post} in {asked:?}");
        }
        drop(asked);

        client.watch(elsewhere);
        until(&client, |view| {
            view.issues.is_none() && view.issue.is_none()
        });
    }

    #[test]
    fn activity_and_blueprints_are_read_on_their_pages_and_a_use_shows_what_it_started() {
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/events?limit=200"] => r#"[{"ts":"2026-11-10T18:01:39Z",
                "key":"playbook:triage:01a1","from":"running","to":"done",
                "reason":{"text":"run finished","truncated":false},"actor":null}]"#
                .into(),
            ["GET", "/api/blueprints"] => r#"[{"id":"triage","description":"Triage issues",
                "owner":"team:infra","rev":"abc","public":true,
                "secrets":[{"declared_name":"gh","carries":false}]}]"#
                .into(),
            ["POST", "/api/blueprints/triage/use"] => r#"{"id":"my-triage","version":1,
                "bindings":[],"placeholders":[{"declared_name":"gh","projection_kind":"env",
                "projection":"GH_TOKEN","reason":"personal_secret"}]}"#
                .into(),
            _ => "[]".into(),
        });
        let elsewhere = Watch {
            controller: true,
            ..Watch::default()
        };

        client.watch(elsewhere.clone());
        let view = until(&client, |view| view.decisions.is_some());
        assert!(view.events.is_none() && view.blueprints.is_none());

        client.watch(Watch {
            activity: true,
            ..elsewhere.clone()
        });
        let view = until(&client, |view| view.events.is_some());
        let events = view.events.unwrap().unwrap();
        assert_eq!(events[0].key, "playbook:triage:01a1");
        assert_eq!(
            events[0].reason.as_ref().map(|r| r.text.as_str()),
            Some("run finished")
        );

        let catalog = Watch {
            blueprints: true,
            ..elsewhere.clone()
        };
        client.watch(catalog);
        let view = until(&client, |view| view.blueprints.is_some());
        assert_eq!(view.blueprints.unwrap().unwrap()[0].secrets.len(), 1);
        client.act(Action::UseBlueprint {
            id: "triage".into(),
            to: "my-triage".into(),
            description: None,
        });
        let view = until(&client, |view| view.used.is_some());
        let used = view.used.unwrap();
        assert_eq!((used.id.as_str(), used.version), ("my-triage", 1));
        assert_eq!(used.placeholders[0].reason, "personal_secret");
        assert_eq!(
            view.acted.map(|a| a.result),
            Some(Ok("Started draft my-triage".to_string()))
        );
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r == "POST /api/blueprints/triage/use")
        );

        client.watch(elsewhere);
        until(&client, |view| view.used.is_none());
    }

    #[test]
    fn the_admin_page_reads_settings_access_autopilot_and_repos_and_sends_each_change() {
        const CONFIG: &str = r#"{"knobs":[{"name":"max_concurrent_pods","value":10,
            "source":"override","overridable":true,"description":"Pods at once."},
            {"name":"allow_t3","value":false,"source":"env","overridable":true}]}"#;
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/config"] | ["PUT", "/api/config/overrides"] => CONFIG.into(),
            ["GET", "/api/access"] => r#"{"admins":["wynn"],"operators":[]}"#.into(),
            ["GET", "/api/autopilot"] | ["POST", "/api/autopilot"] => r#"{"enabled":true}"#.into(),
            ["GET", "/api/repos"] => r#"[{"repo":"llm-d/llm-d","total":9,"watched":true}]"#.into(),
            _ => "{}".into(),
        });
        let elsewhere = Watch {
            controller: true,
            ..Watch::default()
        };

        client.watch(elsewhere.clone());
        let view = until(&client, |view| view.decisions.is_some());
        assert!(view.config.is_none() && view.repos.is_none());

        client.watch(Watch {
            admin: true,
            ..elsewhere
        });
        let view = until(&client, |view| view.config.is_some());
        let knobs = view.config.unwrap().unwrap().knobs;
        assert_eq!(view.access.unwrap().unwrap().admins, ["wynn"]);
        assert!(view.autopilot.unwrap().unwrap().enabled);
        assert_eq!(view.repos.unwrap().unwrap()[0].total, 9);

        let steps = [
            (
                Action::SetOverrides {
                    overrides: crate::overrides::with(&knobs, "allow_t3", "true").unwrap(),
                    justification: "trial".into(),
                },
                "Saved the overrides",
                "PUT /api/config/overrides",
            ),
            (
                Action::SetAutopilot {
                    enabled: false,
                    reason: "deploy".into(),
                },
                "Autopilot off",
                "POST /api/autopilot",
            ),
            (
                Action::Reconcile,
                "Reconcile asked for",
                "POST /api/reconcile",
            ),
            (
                Action::AddRepo {
                    repo: "llm-d/new".into(),
                    justification: "asked".into(),
                },
                "Watching llm-d/new",
                "POST /api/repos",
            ),
            (
                Action::PauseRepo {
                    repo: "llm-d/llm-d".into(),
                    paused: true,
                },
                "Paused llm-d/llm-d",
                "POST /api/repos/llm-d%2Fllm-d/pause",
            ),
            (
                Action::UnwatchRepo {
                    repo: "llm-d/llm-d".into(),
                },
                "Stopped watching llm-d/llm-d",
                "DELETE /api/repos/llm-d%2Fllm-d",
            ),
        ];
        for (action, said, sent) in steps {
            client.act(action);
            until(&client, |view| {
                view.acted
                    .as_ref()
                    .is_some_and(|a| a.result == Ok(said.to_string()))
            });
            assert!(
                requests.lock().unwrap().iter().any(|r| r == sent),
                "{sent} was not sent"
            );
        }
    }

    #[test]
    fn the_policy_tab_reads_the_sets_stores_activates_and_explains() {
        const SET: &str = r#"{"digest":"abc123","text":"permit(principal, action, resource);",
            "created_at":"2026-11-04T21:28:21Z","active":true}"#;
        let (client, requests, _keys) = worker(|line| match line {
            ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
            ["GET", "/api/approvals"] => "{}".into(),
            ["GET", "/api/authz/policy-sets"] => format!("[{SET}]"),
            ["POST", "/api/authz/policy-sets"] => SET.replace("abc123", "def456"),
            ["GET", "/api/authz/explain?login=wynn&action=team%3Aread"] => {
                r#"{"login":"wynn","action":"team:read","allowed":false,"rules":[]}"#.into()
            }
            _ => "{}".into(),
        });
        let elsewhere = Watch {
            controller: true,
            ..Watch::default()
        };

        client.watch(Watch {
            admin: true,
            ..elsewhere.clone()
        });
        let view = until(&client, |view| view.config.is_some());
        assert!(view.policy_sets.is_none(), "policy was read off its tab");

        client.watch(Watch {
            policy: true,
            ..elsewhere.clone()
        });
        let view = until(&client, |view| view.policy_sets.is_some());
        assert!(view.policy_sets.unwrap().unwrap()[0].active);

        client.act(Action::CreatePolicy {
            text: "permit(principal, action, resource);".into(),
        });
        let view = until(&client, |view| view.stored_policy.is_some());
        assert_eq!(view.stored_policy.as_deref(), Some("def456"));
        client.act(Action::ActivatePolicy {
            digest: "def456".into(),
        });
        until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::ActivatePolicy)
        });
        client.act(Action::Explain {
            login: "wynn".into(),
            action: "team:read".into(),
            resource: None,
        });
        let view = until(&client, |view| view.explained.is_some());
        assert!(!view.explained.unwrap().allowed);
        let sent = requests.lock().unwrap().clone();
        for change in [
            "POST /api/authz/policy-sets",
            "POST /api/authz/policy-sets/def456/activate",
        ] {
            assert!(sent.iter().any(|r| r == change), "{change} was not sent");
        }

        client.watch(elsewhere);
        until(&client, |view| {
            view.stored_policy.is_none() && view.explained.is_none()
        });
    }

    fn summary(id: &str) -> dto::DecisionSummary {
        dto::DecisionSummary {
            id: id.into(),
            run_id: "run-1".into(),
            task: "review".into(),
            launch_key: Some("playbook:demo:1".into()),
            questions: vec!["ship".into()],
            opened_at: "2026-11-08T10:00:00Z".into(),
            expires_at: "2026-11-09T10:00:00Z".into(),
        }
    }

    #[test]
    fn the_first_queue_announces_nothing_and_later_ones_only_what_is_new() {
        let mut desk = Desk::default();
        assert!(desk.arrived(&[summary("a")]).is_empty());
        let next = [summary("a"), summary("b")];
        let new: Vec<&str> = desk.arrived(&next).iter().map(|d| d.id.as_str()).collect();
        assert_eq!(new, ["b"]);
        assert!(desk.arrived(&next).is_empty(), "announced once");
        assert!(desk.arrived(&[]).is_empty());
        let returned = [summary("a")];
        let back: Vec<&str> = desk
            .arrived(&returned)
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(
            back,
            ["a"],
            "a request that left the queue and came back is new again"
        );
    }

    #[test]
    fn a_single_answer_question_keeps_one_pick_and_a_multiple_one_keeps_many() {
        let mut desk = Desk::default();
        desk.toggle_pick("ship", "yes", false);
        desk.toggle_pick("ship", "no", false);
        assert_eq!(desk.picks["ship"], ["no"]);
        desk.toggle_pick("ship", "no", false);
        assert!(desk.picks["ship"].is_empty(), "picking it again clears it");
        desk.toggle_pick("tags", "a", true);
        desk.toggle_pick("tags", "b", true);
        assert_eq!(desk.picks["tags"], ["a", "b"]);
        desk.toggle_pick("tags", "a", true);
        assert_eq!(desk.picks["tags"], ["b"]);
    }

    #[test]
    fn picking_another_decision_drops_the_picks() {
        let mut desk = Desk::default();
        desk.pick_decision("a");
        desk.toggle_pick("ship", "yes", false);
        desk.pick_decision("a");
        assert_eq!(desk.picks.len(), 1, "the same request keeps them");
        desk.pick_decision("b");
        assert!(desk.picks.is_empty());
    }

    fn form(kind: &str, name: &str, value: &str) -> SecretForm {
        let mut form = SecretForm::new("team:infra");
        form.kind = SECRET_KINDS
            .iter()
            .position(|(k, _)| *k == kind)
            .expect("kind");
        form.name = TextEdit::new(name);
        form.value = TextEdit::new(value);
        form
    }

    #[test]
    fn a_secret_form_sends_the_registration_the_controller_takes() {
        let body = form("opaque", "gh-token", " ghp_x \n")
            .body()
            .expect("valid");
        assert_eq!(
            body,
            serde_json::json!({"name": "gh-token", "owner": "team:infra", "kind": "opaque",
                "visibility": "broker_only", "value": "ghp_x"})
        );
        let mut visible = form("opaque", "t", "v");
        visible.agent_visible = true;
        assert_eq!(
            visible.body().expect("valid")["visibility"],
            "agent_visible"
        );
        let mut file = form("file", "conf", "a\nb\n");
        file.agent_visible = true;
        let body = file.body().expect("valid");
        assert_eq!(body["value"], "a\nb\n", "a document keeps its bytes");
        assert_eq!(
            body["visibility"], "broker_only",
            "only a token can be agent visible"
        );
        let key = form("inference_api_key", "openai", "sk-1")
            .body()
            .expect("valid");
        assert_eq!(key["value"], r#"{"OPENAI_API_KEY":"sk-1"}"#);
    }

    #[test]
    fn a_secret_form_refuses_what_the_controller_would() {
        assert!(form("opaque", "", "v").body().is_err());
        assert!(form("opaque", "Upper", "v").body().is_err());
        assert!(form("opaque", "has space", "v").body().is_err());
        assert!(form("opaque", &"a".repeat(97), "v").body().is_err());
        assert!(form("opaque", "ok", "").body().is_err());
        let mut key = form("inference_api_key", "k", "sk");
        key.variable = TextEdit::new("openai key");
        assert!(key.body().is_err());
    }

    #[test]
    fn a_new_schedule_body_carries_the_form_and_defaults() {
        let mut form = ScheduleForm::new("team:infra");
        form.cron = TextEdit::new(" 0  6 * * 1 ");
        form.tz = TextEdit::new("America/New_York");
        let body = form
            .body(
                &crate::live::Target::Draft("cve".into()),
                &serde_json::json!({"limit": 5, "repo": "a/b"}),
                3.0,
                "45m",
                Some("sha256:d"),
            )
            .expect("valid");
        assert_eq!(
            body,
            serde_json::json!({"advance_dedupe": true, "enabled": true, "owner": "team:infra",
                "playbook": "cve", "target_kind": "draft_head",
                "params": {"limit": "5", "repo": "a/b"}, "max_cost": 3.0, "max_time": "45m",
                "cron_expr": "0 6 * * 1", "tz": "America/New_York",
                "schema_digest": "sha256:d"})
        );
    }

    #[test]
    fn an_edited_schedule_keeps_what_the_form_does_not_show() {
        let stored: dto::Schedule = serde_json::from_str(
            r#"{"id":"s1","playbook":"report","cron_expr":"0 9 * * *","enabled":false,
                "next_due_at":null,"last_fired_at":null,"consecutive_failures":0,
                "owner_principal":"user:wren","target_kind":"adopted","params":{"a":"1"},
                "max_cost":1.0,"max_time":"10m","advance_dedupe":false,"tz":"UTC",
                "dispatch_target":"wharf","agent_provider":"vertex","agent_model":"m",
                "cursor":{"from":"result","param":"since","path":null}}"#,
        )
        .expect("wire shape");
        let mut form = ScheduleForm::editing(&stored);
        form.owner = "team:infra".into();
        let body = form
            .body(
                &crate::live::Target::Playbook("report".into()),
                &serde_json::json!({"a": "2"}),
                2.0,
                "20m",
                None,
            )
            .expect("valid");
        assert_eq!(body["owner"], "team:infra");
        assert_eq!(body["params"], serde_json::json!({"a": "2"}));
        assert_eq!(body["enabled"], false, "editing does not resume it");
        assert_eq!(body["advance_dedupe"], false);
        assert_eq!(body["dispatch_target"], "wharf");
        assert_eq!(body["provider"], "vertex");
        assert_eq!(body["cursor"]["param"], "since");
        assert_eq!(body["target_kind"], "adopted");
    }

    #[test]
    fn a_test_fire_form_sends_the_cursor_it_shows_and_waits_for_a_typed_value() {
        let mut form = TestFireForm::on("sch-1");
        assert_eq!(form.ask(), Some(dto::TestCursor::Stored));
        form.from = CursorFrom::None;
        assert_eq!(form.ask(), Some(dto::TestCursor::None));
        form.from = CursorFrom::Value;
        assert_eq!(form.ask(), None);
        form.value = TextEdit::new(" {\"seen\": []}\n");
        assert_eq!(
            form.ask(),
            Some(dto::TestCursor::Value {
                value: " {\"seen\": []}\n".to_string()
            }),
            "a file body goes as typed"
        );
    }

    #[test]
    fn a_schedule_form_refuses_a_bad_cron_or_no_zone() {
        let target = crate::live::Target::Playbook("p".into());
        let params = serde_json::json!({});
        let mut form = ScheduleForm::new("user:me");
        form.cron = TextEdit::new("0 9 * *");
        assert!(form.body(&target, &params, 1.0, "5m", None).is_err());
        form.cron = TextEdit::new("0 9 * * *");
        form.tz = TextEdit::new(" ");
        assert!(form.body(&target, &params, 1.0, "5m", None).is_err());
    }

    #[test]
    fn a_share_ends_at_the_end_of_its_day_or_never() {
        assert_eq!(not_after(" "), Ok(None));
        assert_eq!(
            not_after("2026-12-31"),
            Ok(Some("2026-12-31T23:59:59Z".to_string()))
        );
        assert!(not_after("next week").is_err());
        assert!(not_after("2026-02-30").is_err());
    }

    #[test]
    fn a_grantee_is_a_user_or_a_team() {
        assert!(valid_grantee("user:wren"));
        assert!(valid_grantee(" team:infra "));
        for bad in ["wren", "user:", "group:x", "user:a b", "user:a:b", ""] {
            assert!(!valid_grantee(bad), "{bad}");
        }
    }

    #[test]
    fn times_read_relative_to_now() {
        let now = "2026-11-08T12:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_second();
        assert_eq!(relative("2026-11-08T15:00:00Z", now), "in 3h");
        assert_eq!(relative("2026-11-08T11:48:00Z", now), "12m ago");
        assert_eq!(relative("2026-11-03T12:00:00Z", now), "5d ago");
        assert_eq!(relative("2026-11-08T12:00:30Z", now), "now");
        assert_eq!(relative("soon", now), "");
    }

    /// The fixture request as `id`, asking only its first question.
    fn asking_one(id: &str) -> dto::Decision {
        let mut decision = fixture().expect("fixture");
        decision.id = id.into();
        decision.questions.truncate(1);
        decision
    }

    /// [`asking_one`] as the controller sends it.
    fn asking_one_json(id: &str) -> String {
        let mut decision: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/decision.json")).unwrap();
        decision["id"] = id.into();
        decision["questions"].as_array_mut().unwrap().truncate(1);
        decision.to_string()
    }

    fn queue_json(ids: &[&str]) -> String {
        let open: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "run_id": "run-1",
                    "task": "review_bumps",
                    "questions": ["ship"],
                    "opened_at": "2026-11-08T10:00:00Z",
                    "expires_at": "2026-11-09T10:00:00Z",
                })
            })
            .collect();
        serde_json::json!({ "open": open }).to_string()
    }

    #[test]
    fn a_request_asking_one_single_answer_question_gets_a_button_for_each_label() {
        let decision = asking_one("d1");
        assert_eq!(
            app::answers(&decision, "mpp"),
            Some(crate::notify::Answers {
                profile: "mpp".into(),
                decision: "d1".into(),
                question: "ship".into(),
                digest: "sha256:fixture".into(),
                options: vec!["yes".into(), "no".into()],
            })
        );
        let with = |change: fn(&mut dto::Decision)| {
            let mut changed = asking_one("d1");
            change(&mut changed);
            app::answers(&changed, "mpp")
        };
        assert_eq!(app::answers(&fixture().expect("fixture"), "mpp"), None);
        assert_eq!(with(|d| d.questions[0].multiple = true), None);
        assert_eq!(with(|d| d.questions[0].options.clear()), None);
        assert_eq!(with(|d| d.can_answer = false), None);
        assert_eq!(with(|d| d.state = "answered".into()), None);
        assert_eq!(
            with(|d| d.questions[0].options = (0..=app::BUTTONS).map(|n| n.to_string()).collect()),
            None
        );
        assert_eq!(
            with(|d| d.questions[0].options = (0..app::BUTTONS).map(|n| n.to_string()).collect())
                .map(|answers| answers.options.len()),
            Some(app::BUTTONS)
        );
    }

    #[test]
    fn a_request_is_announced_with_buttons_once_it_was_read_in_full() {
        let decision = asking_one("d1");
        let plain = app::note(&summary("d1"), None, "mpp");
        assert_eq!(plain.buttons, crate::notify::Buttons::None);
        assert_eq!(plain.id, "d1");
        let asking = app::note(&summary("d1"), Some(&decision), "mpp");
        assert_eq!(
            Some(asking.buttons),
            app::answers(&decision, "mpp").map(crate::notify::Buttons::Answers)
        );
        assert_eq!(
            (asking.title, asking.body, asking.link),
            (plain.title, plain.body, plain.link)
        );
    }

    #[test]
    fn a_pass_reads_the_first_few_requests_it_has_not_read() {
        let open: Vec<dto::DecisionSummary> = ["a", "b", "c", "d", "e", "f"]
            .map(summary)
            .into_iter()
            .collect();
        let read = BTreeMap::from([("b".to_string(), asking_one("b"))]);
        assert_eq!(client::unread(&open, &read), ["a", "c", "d", "e"]);
        assert_eq!(client::unread(&open, &read).len(), client::DETAILS);
        assert!(client::unread(&open[1..2], &read).is_empty());
    }

    #[test]
    fn an_answer_sent_from_the_page_drops_its_picks_and_a_refused_one_keeps_them() {
        let mut desk = Desk::default();
        desk.pick_decision("a");
        desk.toggle_pick("ship", "yes", false);
        assert_eq!(desk.answered(&Err("expired".into())), None);
        assert_eq!(desk.picks["ship"], ["yes"]);
        assert_eq!(desk.answered(&Ok("Answered".into())), None);
        assert!(desk.picks.is_empty());
    }

    #[test]
    fn a_notification_answer_leaves_another_requests_picks_and_reports_a_refusal() {
        let mut desk = Desk::default();
        desk.pick_decision("a");
        desk.toggle_pick("ship", "yes", false);
        desk.quick = Some("b".into());
        assert_eq!(desk.answered(&Ok("Answered".into())), None);
        assert_eq!(desk.picks["ship"], ["yes"], "they are for request a");
        assert_eq!(desk.quick, None);

        desk.quick = Some("b".into());
        assert_eq!(
            desk.answered(&Err("expired".into())),
            Some(("b".to_string(), "expired".to_string()))
        );
        assert_eq!(desk.picks["ship"], ["yes"]);
        assert_eq!(desk.quick, None);

        desk.quick = Some("a".into());
        assert_eq!(desk.answered(&Ok("Answered".into())), None);
        assert!(desk.picks.is_empty(), "the request shown was answered");
    }

    #[test]
    fn open_requests_are_read_in_full_once_and_forgotten_when_they_close() {
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closing = Arc::clone(&closed);
        let (client, requests, _keys) = worker(move |line| match line {
            ["GET", "/api/decisions"] => {
                if closing.load(std::sync::atomic::Ordering::SeqCst) {
                    queue_json(&[])
                } else {
                    queue_json(&["d1"])
                }
            }
            ["GET", "/api/decisions/d1"] => asking_one_json("d1"),
            ["GET", "/api/approvals"] => "{}".into(),
            _ => "[]".into(),
        });
        let watch = Watch {
            controller: true,
            ..Watch::default()
        };
        client.watch(watch.clone());
        let view = until(&client, |view| view.details.contains_key("d1"));
        assert_eq!(view.details["d1"], asking_one("d1"));
        assert_eq!(
            view.decisions.map(|open| open.map(|open| open.len())),
            Some(Ok(1)),
            "the queue and the request arrive in one view"
        );

        client.watch(Watch {
            schedules: true,
            ..watch.clone()
        });
        let again = until(&client, |view| view.schedules.is_some());
        assert!(again.details.contains_key("d1"));
        let reads = |requests: &Mutex<Vec<String>>| {
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|line| *line == "GET /api/decisions/d1")
                .count()
        };
        assert_eq!(reads(&requests), 1, "a later pass does not read it again");

        closed.store(true, std::sync::atomic::Ordering::SeqCst);
        client.watch(watch);
        until(&client, |view| {
            view.details.is_empty() && view.decisions == Some(Ok(Vec::new()))
        });
    }

    #[test]
    fn a_notification_button_answers_its_request_over_the_wire() {
        use crate::notify::{Answer, Clicked};

        let sent = Arc::new(Mutex::new(Vec::<String>::new()));
        let took = Arc::clone(&sent);
        let (url, _) = crate::testserver::serve_replies(move |asked| {
            let line: Vec<&str> = asked.line.split_whitespace().take(2).collect();
            crate::testserver::Reply::ok(match line.as_slice() {
                ["GET", "/api/decisions"] => queue_json(&["d1"]),
                ["GET", "/api/decisions/d1"] => asking_one_json("d1"),
                ["POST", "/api/decisions/d1/answer"] => {
                    took.lock().unwrap().push(asked.body.clone());
                    asking_one_json("d1")
                }
                ["GET", "/api/approvals"] => "{}".into(),
                _ => "[]".into(),
            })
        });
        let keys = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let target = crate::testserver::target("t", &url, keys.path());
        let mut remote = crate::remote::Remote::new(target, &net, None, None);
        remote.admin_client.watch(Watch {
            controller: true,
            ..Watch::default()
        });
        let view = until(&remote.admin_client, |view| view.details.contains_key("d1"));
        let open = view.decisions.clone().unwrap().unwrap();
        let note = app::note(&open[0], view.details.get("d1"), "t");
        remote.admin = view;
        let Some(Clicked::Answer(answer)) = crate::notify::freedesktop::asked(&note, "answer:yes")
        else {
            panic!("the note has no yes button: {note:?}");
        };

        let elsewhere = Answer {
            profile: "another".into(),
            ..answer.clone()
        };
        assert!(
            !remote.answer(&elsewhere),
            "it was asked on another profile"
        );
        let gone = Answer {
            decision: "d0".into(),
            ..answer.clone()
        };
        assert!(!remote.answer(&gone), "that request no longer waits");
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(remote.desk.quick, None);

        assert!(remote.answer(&answer));
        assert_eq!(remote.desk.quick.as_deref(), Some("d1"));
        assert!(!remote.answer(&answer), "one action at a time");
        let done = until(&remote.admin_client, |view| view.acted.is_some());
        assert_eq!(
            done.acted.map(|acted| (acted.did, acted.result)),
            Some((Did::Answer, Ok("Answered".to_string())))
        );
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&sent[0]).unwrap(),
            serde_json::json!({
                "labels": { "ship": ["yes"] },
                "evidence_digest": "sha256:fixture",
                "note": null,
            })
        );
    }
}

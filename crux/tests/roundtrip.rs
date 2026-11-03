//! Every operation both surfaces share decodes to the same value from its CLI argv and from its
//! MCP arguments, so a field present on one surface and not the other is a compile-time shape,
//! never a silent default.

use clap::Parser;
use crux::ops;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::PathBuf;

#[derive(Parser)]
struct Argv<O: clap::Args> {
    #[command(flatten)]
    op: O,
}

/// Parse `argv` with clap and `args` with serde and require the same operation from both.
fn same<O: clap::Args + DeserializeOwned + PartialEq + Debug>(argv: &[&str], args: Value) -> O {
    let from_cli = Argv::<O>::try_parse_from(std::iter::once("crux").chain(argv.iter().copied()))
        .unwrap_or_else(|e| panic!("{argv:?}: {e}"))
        .op;
    let from_mcp: O =
        serde_json::from_value(args.clone()).unwrap_or_else(|e| panic!("{args}: {e}"));
    assert_eq!(from_cli, from_mcp, "clap {argv:?} vs serde {args}");
    from_cli
}

#[test]
fn whoami() {
    same::<ops::Whoami>(&[], json!({}));
}

#[test]
fn issues() {
    let op = same::<ops::issues::Issues>(
        &[
            "--kind", "scenario", "--status", "parked", "--limit", "7", "--json",
        ],
        json!({"kind": "scenario", "status": "parked", "limit": 7, "json": true}),
    );
    assert_eq!(op.limit, Some(7));
    same::<ops::issues::Issues>(&[], json!({}));
}

#[test]
fn issue() {
    let op = same::<ops::issues::Issue>(
        &["owner/repo#12", "--body-max", "40"],
        json!({"key": "owner/repo#12", "body_max": 40}),
    );
    assert_eq!(op.body_max, 40);
    let defaulted = same::<ops::issues::Issue>(&["k"], json!({"key": "k"}));
    assert_eq!(defaulted.body_max, 6000);
}

#[test]
fn park() {
    same::<ops::issues::Park>(
        &["k", "--reason", "why", "--json"],
        json!({"key": "k", "reason": "why", "json": true}),
    );
}

#[test]
fn unpark() {
    same::<ops::issues::Unpark>(
        &["k", "--reason", "go"],
        json!({"key": "k", "reason": "go"}),
    );
    same::<ops::issues::Unpark>(&["k"], json!({"key": "k"}));
}

#[test]
fn bump() {
    let op = same::<ops::issues::Bump>(
        &["k", "--priority", "7", "--reason", "r"],
        json!({"key": "k", "priority": 7, "reason": "r"}),
    );
    assert_eq!(op.priority, 7);
    same::<ops::issues::Bump>(&["k"], json!({"key": "k", "priority": 100}));
}

#[test]
fn reconcile() {
    same::<ops::issues::Reconcile>(&["--json"], json!({"json": true}));
    same::<ops::issues::Reconcile>(&[], json!({}));
}

#[test]
fn redispatch() {
    same::<ops::issues::Redispatch>(
        &["k", "--justification", "j"],
        json!({"key": "k", "justification": "j"}),
    );
}

#[test]
fn turns() {
    same::<ops::issues::Turns>(
        &["--issue", "k", "--kind", "scope", "--state", "failed"],
        json!({"issue": "k", "kind": "scope", "state": "failed"}),
    );
}

#[test]
fn turn() {
    same::<ops::issues::Turn>(&["pod-1", "--json"], json!({"pod": "pod-1", "json": true}));
}

#[test]
fn contracts() {
    same::<ops::issues::Contracts>(&["--json"], json!({"json": true}));
}

#[test]
fn approve() {
    same::<ops::issues::Approve>(&["k"], json!({"key": "k"}));
}

#[test]
fn approval_detail() {
    same::<ops::issues::ApprovalDetail>(&["k", "--json"], json!({"key": "k", "json": true}));
}

#[test]
fn approvals() {
    same::<ops::issues::Approvals>(&[], json!({}));
}

#[test]
fn runs() {
    same::<ops::runs::Runs>(
        &[
            "--status",
            "running",
            "--repo",
            "o/r",
            "--dispatch-target",
            "wharf",
            "--limit",
            "3",
        ],
        json!({"status": "running", "repo": "o/r", "dispatch_target": "wharf", "limit": 3}),
    );
}

#[test]
fn run() {
    same::<ops::runs::Run>(&["r1"], json!({"run_id": "r1"}));
}

#[test]
fn run_log() {
    let op = same::<ops::runs::RunLog>(
        &["r1", "--cursor", "12", "--limit", "20"],
        json!({"run_id": "r1", "cursor": 12, "limit": 20}),
    );
    assert_eq!((op.cursor, op.limit), (Some(12), Some(20)));
    assert!(
        !op.follow,
        "follow is the CLI's own; the tool never sees it"
    );
    let from_mcp: ops::runs::RunLog = serde_json::from_value(json!({"run_id": "r1"})).unwrap();
    assert_eq!(from_mcp.cursor, None, "the tool starts from the first line");
    assert_eq!(
        from_mcp.interval, 5,
        "a skipped field takes the CLI default"
    );
}

#[test]
fn graph() {
    same::<ops::runs::Graph>(
        &["r1", "--mermaid"],
        json!({"run_id": "r1", "mermaid": true}),
    );
}

#[test]
fn playbooks() {
    same::<ops::playbooks::Playbooks>(&["--json"], json!({"json": true}));
}

#[test]
fn playbook_caps() {
    same::<ops::playbooks::PlaybookCaps>(&[], json!({}));
}

#[test]
fn playbook_schema() {
    same::<ops::playbooks::PlaybookSchema>(&["survey"], json!({"id": "survey"}));
}

#[test]
fn launch() {
    let op = same::<ops::playbooks::Launch>(
        &[
            "survey",
            "--params",
            r#"{"topic":"x"}"#,
            "--max-cost",
            "4",
            "--max-time",
            "30m",
            "--dispatch-target",
            "wharf",
            "--provider",
            "openai",
            "--model",
            "m",
        ],
        json!({
            "id": "survey",
            "params": {"topic": "x"},
            "max_cost": 4.0,
            "max_time": "30m",
            "dispatch_target": "wharf",
            "provider": "openai",
            "model": "m",
        }),
    );
    assert_eq!(op.params, json!({"topic": "x"}));
    let bare = same::<ops::playbooks::Launch>(
        &["survey", "--max-cost", "1", "--max-time", "5m"],
        json!({"id": "survey", "params": {}, "max_cost": 1.0, "max_time": "5m"}),
    );
    assert_eq!(bare.params, json!({}), "the CLI's default params object");
    let from_mcp: ops::playbooks::Launch =
        serde_json::from_value(json!({"id": "s", "max_cost": 1.0, "max_time": "5m"})).unwrap();
    assert_eq!(from_mcp.params, Value::Null, "the tool's omitted params");
    assert_eq!(from_mcp.rerun_of, None, "a rerun is the CLI's own");
}

#[test]
fn playbook_runs() {
    same::<ops::playbooks::PlaybookRuns>(
        &["--status", "done", "--playbook", "survey"],
        json!({"status": "done", "playbook": "survey"}),
    );
}

#[test]
fn playbook_run() {
    same::<ops::playbooks::PlaybookRun>(&["k"], json!({"key": "k"}));
    let from_mcp: ops::playbooks::PlaybookRun =
        serde_json::from_value(json!({"key": "k", "json": true})).unwrap();
    assert!(
        from_mcp.json,
        "the tool's flag parses and is ignored: the reply is always JSON"
    );
}

#[test]
fn playbook_import() {
    same::<ops::playbooks::PlaybookImport>(
        &[
            "o/packs",
            "--git-ref",
            "main",
            "--path",
            "p",
            "--variant",
            "v",
            "--id",
            "cal",
            "--description",
            "d",
        ],
        json!({
            "repo": "o/packs",
            "git_ref": "main",
            "path": "p",
            "variant": "v",
            "id": "cal",
            "description": "d",
        }),
    );
}

#[test]
fn draft_files() {
    same::<ops::drafts::DraftFiles>(
        &["cal", "--version", "3"],
        json!({"draft_id": "cal", "version": 3}),
    );
}

#[test]
fn draft_create() {
    same::<ops::drafts::DraftCreate>(
        &["cal", "--description", "d", "--template", "survey"],
        json!({"draft_id": "cal", "description": "d", "template": "survey"}),
    );
}

#[test]
fn draft_pull() {
    let op = same::<ops::drafts::DraftPull>(
        &["cal", "./pack", "--version", "2"],
        json!({"draft_id": "cal", "dir": "./pack", "version": 2}),
    );
    assert_eq!(op.dir, PathBuf::from("./pack"));
}

#[test]
fn draft_push() {
    same::<ops::drafts::DraftPush>(
        &["cal", "./pack", "--base-version", "4"],
        json!({"draft_id": "cal", "dir": "./pack", "base_version": 4}),
    );
}

#[test]
fn draft_preview() {
    same::<ops::drafts::DraftPreview>(&["cal"], json!({"draft_id": "cal"}));
}

#[test]
fn draft_launch() {
    let op = same::<ops::drafts::DraftLaunch>(
        &[
            "cal",
            "--params",
            r#"{"repo":"o/r"}"#,
            "--max-cost",
            "4",
            "--max-time",
            "30m",
            "--schema-digest",
            "sha256:bb",
            "--provider",
            "openai",
        ],
        json!({
            "draft_id": "cal",
            "params": {"repo": "o/r"},
            "max_cost": 4.0,
            "max_time": "30m",
            "schema_digest": "sha256:bb",
            "provider": "openai",
        }),
    );
    assert_eq!(
        op.params,
        BTreeMap::from([("repo".to_string(), "o/r".to_string())])
    );
    same::<ops::drafts::DraftLaunch>(
        &["cal", "--max-cost", "1", "--max-time", "5m"],
        json!({"draft_id": "cal", "max_cost": 1.0, "max_time": "5m"}),
    );
}

#[test]
fn draft_migrate() {
    let op = same::<ops::drafts::DraftMigrate>(
        &["cal", "--to", "2", "--apply", "--json"],
        json!({"draft_id": "cal", "to": 2, "apply": true, "json": true}),
    );
    assert!(op.apply && !op.all);
    let sweep = same::<ops::drafts::DraftMigrate>(
        &["--all", "--owner", "user:alice", "--dry-run"],
        json!({"all": true, "owner": "user:alice", "dry_run": true}),
    );
    assert_eq!(sweep.draft_id, None);
    assert!(!sweep.apply);
}

#[test]
fn draft_delete() {
    same::<ops::drafts::DraftDelete>(&["cal"], json!({"draft_id": "cal"}));
}

#[test]
fn draft_publish() {
    same::<ops::drafts::DraftPublish>(
        &["cal", "--playbook", "survey"],
        json!({"draft_id": "cal", "playbook": "survey"}),
    );
}

#[test]
fn blueprints() {
    same::<ops::blueprints::Blueprints>(&["--json"], json!({"json": true}));
    same::<ops::blueprints::Blueprints>(&[], json!({}));
}

#[test]
fn blueprint_use() {
    let op = same::<ops::blueprints::BlueprintUse>(
        &["mlr-pack", "kim-copy", "--description", "mine"],
        json!({"id": "mlr-pack", "draft_id": "kim-copy", "description": "mine"}),
    );
    assert_eq!(op.to, "kim-copy");
    same::<ops::blueprints::BlueprintUse>(
        &["mlr-pack", "kim-copy"],
        json!({"id": "mlr-pack", "draft_id": "kim-copy"}),
    );
}

#[test]
fn draft_graduate() {
    same::<ops::drafts::DraftGraduate>(
        &["cal", "--repo", "o/packs", "--path", "p"],
        json!({"draft_id": "cal", "repo": "o/packs", "path": "p"}),
    );
}

#[test]
fn secrets() {
    same::<ops::secrets::Secrets>(&["--json"], json!({"json": true}));
}

#[test]
fn schedules() {
    same::<ops::launches::Schedules>(&[], json!({}));
}

#[test]
fn watches() {
    same::<ops::launches::Watches>(&["--json"], json!({"json": true}));
}

#[test]
fn watch() {
    same::<ops::launches::Watch>(&["w1"], json!({"id": "w1"}));
}

#[test]
fn webhooks() {
    same::<ops::launches::Webhooks>(&[], json!({}));
}

#[test]
fn webhook() {
    same::<ops::launches::Webhook>(&["h1"], json!({"id": "h1"}));
}

#[test]
fn webhook_deliveries() {
    let op = same::<ops::launches::WebhookDeliveries>(
        &["h1", "--json"],
        json!({"id": "h1", "json": true}),
    );
    assert_eq!((op.before, op.limit), (None, None));
    let paged = Argv::<ops::launches::WebhookDeliveries>::try_parse_from([
        "crux", "h1", "--before", "d9", "--limit", "5",
    ])
    .unwrap()
    .op;
    assert_eq!(
        paged.before.as_deref(),
        Some("d9"),
        "paging is the CLI's own"
    );
    let from_mcp: ops::launches::WebhookDeliveries =
        serde_json::from_value(json!({"id": "h1", "before": "d9", "limit": 5})).unwrap();
    assert_eq!(from_mcp.before, None, "the tool cannot page");
}

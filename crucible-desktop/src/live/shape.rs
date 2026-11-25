//! Turning crux's answers into what the views draw: folds, labels and times.

use crate::captured::Captured;
use crate::live::{Identity, LaunchStatus, LiveDetail, LiveDraft, LivePlaybook};
use crate::logline::LogLine;
use crate::model::Verdict;
use crate::net::{Connection, within};
use crate::pack::PackFiles;
use crate::session::RunFold;
use crucible_contract::session::SessionEvent;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;
use std::collections::HashSet;

pub(in crate::live) async fn captured_files(
    connection: &Connection,
    run_id: &str,
) -> Result<Vec<Captured>, String> {
    within("the captured files", connection.client.run_files(run_id))
        .await
        .map(captured)
}

pub(in crate::live) fn captured(listing: crux::dto::RunFiles) -> Vec<Captured> {
    listing.files.into_iter().map(Captured::from).collect()
}

/// A run's graph read as the session it was built from: its plan, then each result. The
/// controller writes absent numbers as `null`, which the session's wire types do not take.
pub(in crate::live) fn graph_fold(graph: &Value) -> RunFold {
    let mut fold = RunFold::default();
    if graph
        .get("tasks")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return fold;
    }
    let mut plan = serde_json::json!({
        "kind": "plan_admitted",
        "plan_version": graph.get("plan_version").and_then(Value::as_u64).unwrap_or(1),
        "budget_usd": 0.0,
        "tasks": graph.get("tasks").cloned().unwrap_or(Value::Array(Vec::new())),
    });
    strip_nulls(&mut plan);
    if let Ok(event) = serde_json::from_value::<SessionEvent>(plan) {
        fold.apply(event);
    }
    for result in graph
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut result = result.clone();
        strip_nulls(&mut result);
        if let Value::Object(fields) = &mut result {
            fields.insert("kind".into(), Value::String("task_result".into()));
        }
        if let Ok(event) = serde_json::from_value::<SessionEvent>(result) {
            fold.apply(event);
        }
    }
    fold
}

fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, field| !field.is_null());
            fields.values_mut().for_each(strip_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// `repo@rev path`, or the draft a registered playbook was published from.
pub(in crate::live) fn pin_label(playbook: &crux::dto::Playbook) -> String {
    let rev: String = playbook.rev.chars().take(12).collect();
    match &playbook.source {
        crux::dto::PlaybookSource::Git { repo, path, .. } => {
            let path = if path.is_empty() {
                String::new()
            } else {
                format!(" {path}")
            };
            format!("{repo}@{rev}{path}")
        }
        crux::dto::PlaybookSource::Draft { draft, version } => {
            format!("published from draft {draft} v{version}")
        }
    }
}

pub(in crate::live) fn identity(whoami: crux::dto::Whoami) -> Identity {
    let user = whoami.user.unwrap_or_default();
    let owners = (!user.is_empty())
        .then(|| format!("user:{user}"))
        .into_iter()
        .chain(whoami.teams.iter().map(|t| format!("team:{}", t.team)))
        .collect();
    Identity {
        user,
        role: whoami.role,
        owners,
        autoresearch: whoami.entitlements.iter().any(|e| e == "autoresearch"),
    }
}

/// The listed drafts `identity` owns, as themself or as a team, that are not retired, newest
/// save first.
pub(in crate::live) fn owned(
    drafts: Vec<crux::dto::PlaybookDraft>,
    identity: &Identity,
) -> Vec<LiveDraft> {
    let mut drafts: Vec<LiveDraft> = drafts
        .into_iter()
        .filter(|draft| draft.retired_at.is_none())
        .filter(|draft| {
            identity
                .owners
                .iter()
                .any(|owner| owner.eq_ignore_ascii_case(&draft.owner))
        })
        .map(|draft| LiveDraft {
            by: draft.created_by.unwrap_or_else(|| draft.owner.clone()),
            id: draft.id,
            description: Some(draft.description).filter(|text| !text.is_empty()),
            owner: draft.owner,
            version: draft.latest_version,
            compiles: draft.compiles,
            saved: epoch(&draft.updated_at).unwrap_or_default(),
        })
        .collect();
    drafts.sort_by_key(|draft| std::cmp::Reverse(draft.saved));
    drafts
}

/// The drafts none of `launches` ran.
pub(in crate::live) fn unlaunched(
    drafts: &[LiveDraft],
    launches: &[LivePlaybook],
) -> Vec<LiveDraft> {
    let launched: HashSet<&str> = launches.iter().map(|launch| launch.id.as_str()).collect();
    drafts
        .iter()
        .filter(|draft| !launched.contains(draft.id.as_str()))
        .cloned()
        .collect()
}

pub(in crate::live) fn agent_label(launch: &crux::dto::PlaybookRun) -> String {
    match (
        launch.agent_provider.as_deref(),
        launch.agent_model.as_deref(),
    ) {
        (Some(provider), Some(model)) => format!("{provider} · {model}"),
        (None, Some(model)) => model.to_string(),
        (Some(provider), None) => provider.to_string(),
        (None, None) => String::new(),
    }
}

/// The run's whole console output as the controller serves it, typed.
pub(in crate::live) async fn console(
    connection: &Connection,
    run_id: &str,
) -> Result<Vec<LogLine>, String> {
    within("the console", connection.client.run_log(run_id))
        .await
        .map(typed_log)
}

pub(in crate::live) fn typed_log(log: crux::dto::RunLog) -> Vec<LogLine> {
    log.text
        .map(|text| text.lines().map(LogLine::parse).collect())
        .unwrap_or_default()
}

pub(in crate::live) fn build_detail(
    launch: &LivePlaybook,
    fold: RunFold,
    log: Vec<LogLine>,
) -> LiveDetail {
    let settled = !fold.tasks.is_empty() && fold.required_settled();
    let verdict = match launch.status {
        LaunchStatus::Running => Verdict::Running,
        LaunchStatus::Done if !settled => Verdict::Unfinished,
        LaunchStatus::Done if fold.required_passed() => Verdict::Valid,
        LaunchStatus::Done => Verdict::Invalid,
        LaunchStatus::Error => Verdict::Error,
        LaunchStatus::Cancelled => Verdict::Cancelled,
        LaunchStatus::New => Verdict::Unfinished,
        LaunchStatus::Other(_) if settled && fold.required_passed() => Verdict::Valid,
        LaunchStatus::Other(_) => Verdict::Unfinished,
    };
    let spent = if fold.tasks.iter().all(|task| task.result.is_none()) {
        launch.cost.unwrap_or(0.0)
    } else {
        fold.spent()
    };
    LiveDetail {
        streaming: false,
        key: launch.key.clone(),
        spent,
        verdict,
        passed: fold.passed(),
        failed: fold.failed(),
        fold,
        log,
        source: "",
        error: None,
        launch: None,
        pack: Ok(PackFiles::default()),
        run_id: None,
        files: None,
        calls: Vec::new(),
    }
}

/// `18:00`, the time of day of an RFC 3339 stamp in `zone`; empty when it does not parse.
pub(in crate::live) fn clock(stamp: &str, zone: &TimeZone) -> String {
    stamp
        .parse::<Timestamp>()
        .map(|at| at.to_zoned(zone.clone()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

pub(in crate::live) fn unix_now() -> i64 {
    Timestamp::now().as_second()
}

/// Whole Unix seconds from an RFC 3339 stamp.
pub(in crate::live) fn epoch(stamp: &str) -> Option<i64> {
    stamp.parse::<Timestamp>().ok().map(|at| at.as_second())
}

/// `Nov 6 18:00` in `zone`; the raw stamp when it does not parse.
pub(in crate::live) fn landed_label(stamp: &str, zone: &TimeZone) -> String {
    stamp.parse::<Timestamp>().map_or_else(
        |_| stamp.to_string(),
        |at| {
            at.to_zoned(zone.clone())
                .strftime("%b %-d %H:%M")
                .to_string()
        },
    )
}
